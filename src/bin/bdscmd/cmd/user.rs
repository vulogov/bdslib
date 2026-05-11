//! `bdscmd user <subcommand>` — manage cluster-replicated users.
//!
//! Six subcommands grouped by HMAC requirement:
//!
//! - **Admin** (`add`, `modify`, `delete`, `list`): call HMAC-protected
//!   `v3/user.*` RPCs.  Require `--secret` (or `BDSCMD_CLUSTER_SECRET`)
//!   matching the target node's `cluster.shared_secret`.  Exception:
//!   `add` is admitted unconditionally when the user store is empty
//!   (first-user bootstrap) — `--secret` is optional in that case.
//! - **Public** (`authenticate`): the standard login path.  No secret
//!   needed; rate-limited by the server.
//! - **Local** (`whoami`): verifies a session token offline using only
//!   the cluster shared secret — no RPC.

use anyhow::{bail, Context, Result};
use bdslib::cluster::{hmac_auth, session::verify_session_token};
use clap::{Args, Subcommand};
use serde_json::{json, Map, Value};

#[derive(Args)]
pub struct Cmd {
    /// Cluster shared secret for admin subcommands
    /// (`add`/`modify`/`delete`/`list`).  Same key as
    /// `cluster.shared_secret` in `bds.hjson`.  `whoami` uses it to
    /// verify a session token locally; `authenticate` ignores it.
    #[arg(short, long, env = "BDSCMD_CLUSTER_SECRET", default_value = "")]
    secret: String,

    #[command(subcommand)]
    sub: Sub,
}

#[derive(Subcommand)]
enum Sub {
    /// (admin) Create a new user.  HMAC required EXCEPT for the very
    /// first user in an empty store (first-user bootstrap).
    Add(AddArgs),

    /// (admin) Replace fields on an existing user.  Only the fields
    /// you supply are changed; everything else stays put.
    Modify(ModifyArgs),

    /// (admin) Hard-delete a user (writes a tombstone so anti-entropy
    /// doesn't resurrect it from a peer that hasn't seen the delete).
    Delete(DeleteArgs),

    /// (admin) List every user as a hash-free summary
    /// (id, username, auth_method, metadata, timestamps, disabled).
    /// The credential hash is NEVER returned by this method.
    List,

    /// (public) Verify credentials and obtain a session token.  No
    /// `--secret` needed — this is the standard login path.  Returns
    /// `{ok, user_id, session_token, ttl_secs, expires_at}` on
    /// success or `{ok: false, error: "invalid credentials"}` on any
    /// failure (no info-leak between unknown-user and wrong-password).
    Authenticate(AuthArgs),

    /// (local) Verify a session token offline.  No RPC — uses
    /// `--secret` to recompute and compare the HMAC.  Prints the
    /// decoded claims (user_id, expires_at) when valid; exits with a
    /// non-zero status code otherwise.
    Whoami(WhoamiArgs),
}

#[derive(Args)]
struct AddArgs {
    #[arg(short = 'u', long)]
    username: String,
    /// Plaintext password.  Hashed locally by argon2id on the target
    /// node.  Never logged or stored verbatim.
    #[arg(short = 'p', long)]
    password: String,
    /// Display name to include in the user's metadata (`metadata.display_name`).
    #[arg(short = 'n', long)]
    display_name: Option<String>,
    /// Optional alternative auth method (currently only `password` is
    /// shipped; OAuth/LDAP verifier impls register at startup).
    #[arg(long, default_value = "password")]
    auth_method: String,
}

#[derive(Args)]
struct ModifyArgs {
    #[arg(short = 'i', long)]
    id: String,
    /// Replace the password.  When omitted, the existing hash stays.
    #[arg(short = 'p', long)]
    password: Option<String>,
    /// Replace the display name (metadata.display_name).
    #[arg(short = 'n', long)]
    display_name: Option<String>,
    /// Disable the account (login will fail with invalid credentials).
    #[arg(long, conflicts_with = "enable")]
    disable: bool,
    /// Re-enable a previously disabled account.
    #[arg(long, conflicts_with = "disable")]
    enable: bool,
    /// Switch the auth method (e.g. password → oauth-google).
    #[arg(long)]
    new_auth_method: Option<String>,
}

#[derive(Args)]
struct DeleteArgs {
    /// UUIDv7 of the user.
    #[arg(short = 'i', long)]
    id: String,
}

#[derive(Args)]
struct AuthArgs {
    #[arg(short = 'u', long)]
    username: String,
    #[arg(short = 'p', long)]
    password: String,
}

#[derive(Args)]
struct WhoamiArgs {
    /// Session token previously returned by `user authenticate`.  Same
    /// value as the `bds_session` cookie bdsweb sets after login.
    #[arg(short = 't', long)]
    token: String,
}

// ── HMAC helper ──────────────────────────────────────────────────────────────

fn signed_call(url: &str, method: &str, secret: &str, mut params: Map<String, Value>) -> Result<Value> {
    let canonical = serde_json::to_vec(&Value::Object(params.clone()))
        .context("serialize params for HMAC")?;
    let sig = hmac_auth::sign(secret, &canonical);
    params.insert("_hmac".into(), Value::String(sig));
    crate::client::call(url, method, Value::Object(params))
}

/// Same as `signed_call` but the params are forwarded raw (no HMAC
/// added) — used for `add` in the first-user bootstrap path where the
/// caller omitted `--secret` and the server admits the call unsigned.
fn unsigned_call(url: &str, method: &str, params: Map<String, Value>) -> Result<Value> {
    crate::client::call(url, method, Value::Object(params))
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    match args.sub {
        Sub::Add(a) => {
            let mut params = Map::new();
            params.insert("username".into(), Value::String(a.username));
            params.insert("password".into(), Value::String(a.password));
            params.insert("auth_method".into(), Value::String(a.auth_method));
            if let Some(dn) = a.display_name {
                params.insert("metadata".into(), json!({"display_name": dn}));
            }
            // First-user bootstrap path: when `--secret` is omitted we
            // forward the call unsigned and let the server decide.  If
            // the store is empty it admits the call; otherwise it
            // returns `-32098 missing _hmac field` and the operator
            // knows they need to supply `--secret`.
            if args.secret.is_empty() {
                unsigned_call(url, "v3/user.add", params)
            } else {
                signed_call(url, "v3/user.add", &args.secret, params)
            }
        }

        Sub::Modify(a) => {
            if args.secret.is_empty() {
                bail!("--secret is required for `user modify` \
                       (or set BDSCMD_CLUSTER_SECRET)");
            }
            let mut params = Map::new();
            params.insert("id".into(), Value::String(a.id));
            if let Some(pw) = a.password {
                params.insert("password".into(), Value::String(pw));
            }
            if let Some(dn) = a.display_name {
                params.insert("metadata".into(), json!({"display_name": dn}));
            }
            if a.disable {
                params.insert("disabled".into(), Value::Bool(true));
            }
            if a.enable {
                params.insert("disabled".into(), Value::Bool(false));
            }
            if let Some(m) = a.new_auth_method {
                params.insert("new_auth_method".into(), Value::String(m));
            }
            signed_call(url, "v3/user.modify", &args.secret, params)
        }

        Sub::Delete(a) => {
            if args.secret.is_empty() {
                bail!("--secret is required for `user delete` \
                       (or set BDSCMD_CLUSTER_SECRET)");
            }
            let mut params = Map::new();
            params.insert("id".into(), Value::String(a.id));
            signed_call(url, "v3/user.delete", &args.secret, params)
        }

        Sub::List => {
            if args.secret.is_empty() {
                bail!("--secret is required for `user list` \
                       (or set BDSCMD_CLUSTER_SECRET)");
            }
            signed_call(url, "v3/user.list", &args.secret, Map::new())
        }

        Sub::Authenticate(a) => {
            // Public — no secret.
            crate::client::call(url, "v3/user.authenticate", json!({
                "username": a.username,
                "password": a.password,
            }))
        }

        Sub::Whoami(a) => {
            // Offline verification — no RPC.  Requires `--secret`
            // because the session token's HMAC is computed against the
            // cluster shared secret.
            if args.secret.is_empty() {
                bail!("--secret is required for `user whoami` \
                       (or set BDSCMD_CLUSTER_SECRET)");
            }
            match verify_session_token(&a.token, &args.secret) {
                Ok(claims) => Ok(json!({
                    "ok":         true,
                    "user_id":    claims.user_id.to_string(),
                    "expires_at": claims.expires_at,
                })),
                Err(e) => Ok(json!({
                    "ok":    false,
                    "error": e.to_string(),
                })),
            }
        }
    }
}
