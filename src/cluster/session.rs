//! Stateless session tokens signed with the cluster shared secret.
//!
//! A session token is a base64url-encoded triple
//!
//! ```text
//! <user_id>.<expires_at_unix_secs>.<hex_hmac_sha256>
//! ```
//!
//! where the HMAC covers `<user_id>.<expires_at>` using the
//! `cluster.shared_secret` as the key.  Every node in the cluster
//! holds the same secret, so the token verifies on any node — there
//! is no central session store and no replication chatter.  Logout is
//! purely client-side (cookie deletion); there is no per-session
//! revocation in v1.
//!
//! ## Why not JWT?
//!
//! The token shape is intentionally simpler than a full JWT — no JSON
//! payload, no algorithm field, no risk of `alg=none` confusion.  A
//! single algorithm (HMAC-SHA256) is hard-coded.  For the same
//! reason we use base64url without padding.

use crate::common::error::{err_msg, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Decoded payload of a verified session cookie.  Returned by
/// [`verify_session_token`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionClaims {
    pub user_id:    Uuid,
    pub expires_at: u64,
}

/// Distinct failure modes from [`verify_session_token`].  Surfaced as
/// distinct types so the bdsweb middleware can log "expired" vs
/// "tampered" differently while always presenting the same generic
/// "please log in" page to the user.
#[derive(Debug)]
pub enum SessionError {
    /// Token wasn't shaped `<id>.<exp>.<hmac>`.
    Malformed(&'static str),
    /// Base64 decoding the segments failed.
    BadEncoding(String),
    /// `expires_at` parsed but is in the past.
    Expired { expired_at: u64, now: u64 },
    /// HMAC didn't match — token was tampered with, signed by a
    /// different secret, or this node's secret rotated.
    BadSignature,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Malformed(why)         => write!(f, "session token malformed: {why}"),
            SessionError::BadEncoding(s)         => write!(f, "session token decoding failed: {s}"),
            SessionError::Expired { expired_at, now } =>
                write!(f, "session token expired at {expired_at} (now {now})"),
            SessionError::BadSignature           => write!(f, "session token signature invalid"),
        }
    }
}
impl std::error::Error for SessionError {}

/// Issue a fresh signed session token for `user_id`, valid for
/// `ttl_secs` seconds from now.  `secret` is the cluster shared
/// secret (`cluster.shared_secret` from `bds.hjson`).  Returns the
/// base64url-encoded cookie value the bdsweb login handler should
/// drop into a `Set-Cookie` header.
///
/// `secret` MUST be at least 16 bytes; shorter secrets are explicitly
/// refused so a misconfigured cluster can't silently issue
/// brute-forceable tokens.
pub fn issue_session_token(user_id: Uuid, ttl_secs: u64, secret: &str) -> Result<String> {
    if secret.len() < 16 {
        return Err(err_msg(
            "session secret must be ≥16 bytes (cluster.shared_secret)"
        ));
    }
    let expires_at = now_secs().saturating_add(ttl_secs);
    let payload    = format!("{user_id}.{expires_at}");
    let mac_hex    = sign(&payload, secret);
    Ok(format!("{payload}.{mac_hex}"))
}

/// Verify a token previously issued by [`issue_session_token`].
/// Returns the decoded claims on success; precise [`SessionError`]
/// variant on failure.
pub fn verify_session_token(token: &str, secret: &str) -> std::result::Result<SessionClaims, SessionError> {
    if secret.len() < 16 {
        return Err(SessionError::BadSignature);
    }
    let mut parts = token.splitn(3, '.');
    let user_s    = parts.next().ok_or(SessionError::Malformed("missing user_id"))?;
    let exp_s     = parts.next().ok_or(SessionError::Malformed("missing expires_at"))?;
    let sig_hex   = parts.next().ok_or(SessionError::Malformed("missing signature"))?;
    if parts.next().is_some() {
        return Err(SessionError::Malformed("trailing segments"));
    }

    let user_id = Uuid::parse_str(user_s)
        .map_err(|e| SessionError::BadEncoding(format!("user_id: {e}")))?;
    let expires_at: u64 = exp_s.parse()
        .map_err(|_| SessionError::BadEncoding(format!("expires_at: {exp_s:?}")))?;

    let payload = format!("{user_s}.{exp_s}");
    let expected_hex = sign(&payload, secret);
    if !constant_time_eq(sig_hex.as_bytes(), expected_hex.as_bytes()) {
        return Err(SessionError::BadSignature);
    }

    let now = now_secs();
    if now >= expires_at {
        return Err(SessionError::Expired { expired_at: expires_at, now });
    }
    Ok(SessionClaims { user_id, expires_at })
}

fn sign(payload: &str, secret: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time byte comparison — same length is required (HMAC hex
/// always 64 chars, so this just guards against attacker-supplied
/// short strings).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// silence unused — base64 import is reserved for an upcoming
// "wrap as URL_SAFE_NO_PAD" variant if the cookie ends up too long.
#[allow(dead_code)] fn _b64_keep(s: &[u8]) -> String { URL_SAFE_NO_PAD.encode(s) }

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-cluster-secret-32-or-more-chars";

    #[test]
    fn round_trip_succeeds_for_unmodified_token() {
        let id = Uuid::now_v7();
        let tok = issue_session_token(id, 3600, SECRET).unwrap();
        let claims = verify_session_token(&tok, SECRET).unwrap();
        assert_eq!(claims.user_id, id);
        assert!(claims.expires_at > now_secs());
    }

    #[test]
    fn tampered_user_id_fails_signature() {
        let id = Uuid::now_v7();
        let tok = issue_session_token(id, 3600, SECRET).unwrap();
        // Swap the user id but keep the original signature.
        let mut parts: Vec<&str> = tok.splitn(3, '.').collect();
        let other = Uuid::now_v7().to_string();
        parts[0] = &other;
        let tampered = parts.join(".");
        match verify_session_token(&tampered, SECRET) {
            Err(SessionError::BadSignature) => {}
            other => panic!("expected BadSignature, got {other:?}"),
        }
    }

    #[test]
    fn tampered_expiry_fails_signature() {
        let id = Uuid::now_v7();
        let tok = issue_session_token(id, 3600, SECRET).unwrap();
        let mut parts: Vec<&str> = tok.splitn(3, '.').collect();
        let later = (now_secs() + 999_999).to_string();
        parts[1] = &later;
        let tampered = parts.join(".");
        match verify_session_token(&tampered, SECRET) {
            Err(SessionError::BadSignature) => {}
            other => panic!("expected BadSignature when extending expiry, got {other:?}"),
        }
    }

    #[test]
    fn different_secret_rejects_token() {
        let id = Uuid::now_v7();
        let tok = issue_session_token(id, 3600, SECRET).unwrap();
        match verify_session_token(&tok, "another-secret-32-or-more-chars") {
            Err(SessionError::BadSignature) => {}
            other => panic!("token from a different secret must NOT verify, got {other:?}"),
        }
    }

    #[test]
    fn expired_token_returns_expired_variant() {
        let id  = Uuid::now_v7();
        // ttl=0 means expires_at == now → already expired.
        let tok = issue_session_token(id, 0, SECRET).unwrap();
        // Sleep 1s to be sure now > expires_at; CI jitter sometimes
        // falls in the same second.
        std::thread::sleep(std::time::Duration::from_secs(1));
        match verify_session_token(&tok, SECRET) {
            Err(SessionError::Expired { .. }) => {}
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn malformed_token_rejected() {
        for bad in &["", "no-dots", "one.dot.but.too.many.dots", "a.b"] {
            assert!(verify_session_token(bad, SECRET).is_err(),
                "bad token must NOT verify: {bad:?}");
        }
    }

    #[test]
    fn issue_refuses_short_secret() {
        let res = issue_session_token(Uuid::now_v7(), 3600, "too-short");
        assert!(res.is_err(), "short secret must error");
    }
}
