//! Pluggable credential verification.
//!
//! `UserStorage` rows carry an `auth_method` field (e.g. `"password"`,
//! `"oauth-google"`, `"ldap"`).  At login time the row is fetched and
//! the right [`CredentialVerifier`] impl is dispatched against the
//! presented credential.  This keeps the storage schema invariant
//! across auth methods — adding OAuth or LDAP later means writing one
//! new verifier impl and registering it; no migration.
//!
//! Phase 1 ships only the [`PasswordVerifier`] backed by Argon2id:
//! the OWASP-recommended modern default with `m=19MiB, t=2, p=1`.
//! Other impls land alongside as separate types in this module.

use crate::common::error::{err_msg, Result};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier as _, SaltString},
    Algorithm, Argon2, Params, Version,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Logical authentication method recorded on each user row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AuthMethod {
    /// Local password hashed with Argon2id.
    Password,
    /// Federated OAuth.  Provider name is opaque (e.g. `"google"`,
    /// `"github"`).  `stored_hash` for these rows is whatever the
    /// provider returns as a stable user id (or `""` if the verifier
    /// re-introspects the bearer token on every login).
    OAuth { provider: String },
    /// LDAP / Active Directory.  `server` is the bind URL.
    Ldap { server: String },
    /// Custom verifier registered by the operator under a unique name.
    Custom { name: String },
}

impl AuthMethod {
    /// Wire form stored in the `users.auth_method` column.
    pub fn to_wire(&self) -> String {
        match self {
            AuthMethod::Password           => "password".to_owned(),
            AuthMethod::OAuth { provider } => format!("oauth-{provider}"),
            AuthMethod::Ldap  { server   } => format!("ldap-{server}"),
            AuthMethod::Custom{ name     } => format!("custom-{name}"),
        }
    }

    /// Inverse of [`to_wire`] — used when loading rows from the
    /// `users` table.  Unrecognised prefixes round-trip as
    /// [`AuthMethod::Custom`] so unknown methods don't silently
    /// degrade to `Password` (which would give an attacker a
    /// password-verifier path against an OAuth row).
    pub fn from_wire(s: &str) -> Self {
        if s == "password" {
            AuthMethod::Password
        } else if let Some(rest) = s.strip_prefix("oauth-") {
            AuthMethod::OAuth { provider: rest.to_owned() }
        } else if let Some(rest) = s.strip_prefix("ldap-") {
            AuthMethod::Ldap { server: rest.to_owned() }
        } else if let Some(rest) = s.strip_prefix("custom-") {
            AuthMethod::Custom { name: rest.to_owned() }
        } else {
            AuthMethod::Custom { name: s.to_owned() }
        }
    }
}

/// Credential verifier — one impl per [`AuthMethod`] family.
///
/// Verifiers are owned by a [`VerifierRegistry`] held on the
/// `Cluster` struct (Phase 1e) so the entire process shares a single
/// argon2 / OAuth client setup.
pub trait CredentialVerifier: Send + Sync {
    /// Which method does this verifier handle?  Used by the registry
    /// to pick the right verifier when dispatching from a row.
    fn method(&self) -> AuthMethod;

    /// Verify a presented credential against the row's `stored_hash`.
    /// Returns `Ok(true)` on match, `Ok(false)` on mismatch, `Err`
    /// only when the verifier itself fails (network error for OAuth,
    /// malformed hash, …) — callers MUST treat both `Ok(false)` and
    /// `Err(...)` as a failed login.
    fn verify(&self, stored_hash: &str, presented: &str) -> Result<bool>;

    /// Hash a fresh credential for storage.  For password rows this
    /// is an argon2id hash; for OAuth it returns the provider's user
    /// id (stored verbatim so we can compare on subsequent logins);
    /// for LDAP it returns "" because verification is via bind rather
    /// than stored material.
    fn store(&self, raw: &str) -> Result<String>;
}

/// Default Argon2id-backed [`CredentialVerifier`] — the only one
/// registered by [`VerifierRegistry::default()`] today.  Parameters
/// match the 2024 OWASP recommendation:
///
/// | Parameter | Value | Why |
/// |---|---|---|
/// | `m_cost`  | 19_456 KiB (≈19 MiB) | RAM-hard barrier above what GPUs do well |
/// | `t_cost`  | 2 | Iterations — keeps single-login latency under ~50 ms on a server CPU |
/// | `p_cost`  | 1 | Parallelism — single-thread per-call is plenty for a login path |
/// | `output_len` | 32 bytes | 256-bit hash |
pub struct PasswordVerifier {
    argon2: Argon2<'static>,
}

impl PasswordVerifier {
    pub fn new() -> Self {
        let params = Params::new(
            19_456, // m_cost (KiB)
            2,      // t_cost (iterations)
            1,      // p_cost (parallelism)
            Some(32),
        ).expect("argon2 params are valid");
        Self {
            argon2: Argon2::new(Algorithm::Argon2id, Version::V0x13, params),
        }
    }
}

impl Default for PasswordVerifier {
    fn default() -> Self { Self::new() }
}

impl CredentialVerifier for PasswordVerifier {
    fn method(&self) -> AuthMethod { AuthMethod::Password }

    fn verify(&self, stored_hash: &str, presented: &str) -> Result<bool> {
        let parsed = PasswordHash::new(stored_hash)
            .map_err(|e| err_msg(format!("malformed argon2 hash: {e}")))?;
        Ok(self.argon2.verify_password(presented.as_bytes(), &parsed).is_ok())
    }

    fn store(&self, raw: &str) -> Result<String> {
        let salt = SaltString::generate(&mut OsRng);
        let hash = self.argon2
            .hash_password(raw.as_bytes(), &salt)
            .map_err(|e| err_msg(format!("argon2 hash failed: {e}")))?;
        Ok(hash.to_string())
    }
}

/// Process-wide map of `AuthMethod` → verifier impl.  Held by
/// `Cluster` and consulted by `UserStorage::verify` to dispatch the
/// right check for each user row's recorded method.
///
/// Use [`VerifierRegistry::default()`] for the standard
/// password-only setup.  To add OAuth/LDAP later:
///
/// ```ignore
/// let mut reg = VerifierRegistry::default();
/// reg.register(Arc::new(GoogleOAuthVerifier::new(client)));
/// ```
pub struct VerifierRegistry {
    verifiers: HashMap<String, Arc<dyn CredentialVerifier>>,
}

impl VerifierRegistry {
    pub fn new() -> Self {
        Self { verifiers: HashMap::new() }
    }

    pub fn register(&mut self, v: Arc<dyn CredentialVerifier>) {
        self.verifiers.insert(v.method().to_wire(), v);
    }

    /// Return the verifier for `method`, or `None` when no
    /// implementation is registered (failed login is the right
    /// response to that — never silently fall back to a different
    /// verifier).
    pub fn for_method(&self, method: &AuthMethod) -> Option<Arc<dyn CredentialVerifier>> {
        self.verifiers.get(&method.to_wire()).cloned()
    }

    /// True when the registry knows how to handle `method`.
    pub fn supports(&self, method: &AuthMethod) -> bool {
        self.verifiers.contains_key(&method.to_wire())
    }
}

impl Default for VerifierRegistry {
    /// Standard registry — password-only.  Add OAuth/LDAP impls by
    /// constructing a registry manually and calling `register`.
    fn default() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(PasswordVerifier::new()));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip_succeeds_for_correct_password() {
        let v = PasswordVerifier::new();
        let stored = v.store("hunter2").unwrap();
        assert!(v.verify(&stored, "hunter2").unwrap());
    }

    #[test]
    fn password_verify_fails_for_wrong_password() {
        let v = PasswordVerifier::new();
        let stored = v.store("hunter2").unwrap();
        assert!(!v.verify(&stored, "hunter3").unwrap());
    }

    #[test]
    fn malformed_hash_returns_err_not_panic() {
        let v = PasswordVerifier::new();
        assert!(v.verify("not-an-argon2-hash", "anything").is_err());
    }

    #[test]
    fn auth_method_wire_round_trip() {
        for m in [
            AuthMethod::Password,
            AuthMethod::OAuth { provider: "google".into() },
            AuthMethod::Ldap  { server:   "ldap://corp.example.com".into() },
            AuthMethod::Custom{ name:     "yubikey".into() },
        ] {
            let s = m.to_wire();
            let back = AuthMethod::from_wire(&s);
            assert_eq!(m, back, "round-trip failed for {s:?}");
        }
    }

    #[test]
    fn unknown_wire_form_decodes_as_custom_not_password() {
        // Critical: a typo or future-method label must NEVER decode
        // as Password — that would give a password verifier a path
        // against a non-password row.
        match AuthMethod::from_wire("totally-new-method") {
            AuthMethod::Custom { name } => assert_eq!(name, "totally-new-method"),
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn registry_dispatches_to_password_verifier_by_default() {
        let reg = VerifierRegistry::default();
        let v = reg.for_method(&AuthMethod::Password).expect("password verifier registered");
        let stored = v.store("ok").unwrap();
        assert!(v.verify(&stored, "ok").unwrap());
        assert!(reg.for_method(&AuthMethod::OAuth { provider: "google".into() }).is_none(),
            "OAuth verifier must NOT be silently registered");
    }

    #[test]
    fn two_hashes_of_same_password_differ() {
        // Salt randomness — verifies argon2 isn't deterministic
        // (which would let an attacker batch-test passwords).
        let v = PasswordVerifier::new();
        let a = v.store("same").unwrap();
        let b = v.store("same").unwrap();
        assert_ne!(a, b, "argon2 produced identical hashes — salt isn't being randomised");
    }
}
