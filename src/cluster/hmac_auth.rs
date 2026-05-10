//! Shared-secret HMAC-SHA256 helpers for v3/cluster.* and v3/* RPC auth.
//!
//! All cluster traffic (gossip + replicated writes) carries a hex-encoded
//! HMAC-SHA256 of the request body computed with `cluster.shared_secret`.
//! The verifier uses constant-time comparison.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Sign `payload` with `secret` and return a lowercase hex string.
pub fn sign(secret: &str, payload: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

/// Verify `signature_hex` against the HMAC of `payload` under `secret`.
/// Constant-time comparison via `hmac::Mac::verify_slice`.
pub fn verify(secret: &str, payload: &[u8], signature_hex: &str) -> bool {
    let sig_bytes = match hex::decode(signature_hex) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(payload);
    mac.verify_slice(&sig_bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let sig = sign("hunter2hunter2!!", b"hello");
        assert!(verify("hunter2hunter2!!", b"hello", &sig));
        assert!(!verify("hunter2hunter2!!", b"world", &sig));
        assert!(!verify("wrong-secret-key", b"hello", &sig));
    }

    #[test]
    fn rejects_garbage_signature() {
        assert!(!verify("k", b"x", "not-hex"));
        assert!(!verify("k", b"x", ""));
    }
}
