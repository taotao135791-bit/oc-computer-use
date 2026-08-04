//! Session control tokens: capability credentials for session ownership.
//!
//! A `control_token` is issued **once**, when a session is created, and is the
//! only thing that authorizes mutating operations on that session (act, cancel,
//! pause, resume, takeover, release, stop). Knowing the session id grants
//! nothing; the daemon verifies a hash of the token before any side effect.
//!
//! Hygiene rules enforced here:
//! - tokens are 32 random bytes from the OS CSPRNG, base64url-encoded;
//! - the runtime stores only a SHA-256 hash, never the plaintext;
//! - `Debug` on any token-bearing type redacts the value;
//! - hashes are compared without short-circuiting (constant-time-ish).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Wire protocol version. Round 3 introduced control tokens and server-side
/// ownership enforcement, which is a breaking protocol change; adapters check
/// this before talking to a daemon.
pub const PROTOCOL_VERSION: u32 = 2;

/// Random bytes per token (256 bits).
const TOKEN_BYTES: usize = 32;

/// A plaintext control token. Never serialized, never logged; `Debug` prints
/// `[REDACTED]`.
#[derive(Clone, PartialEq, Eq)]
pub struct ControlToken(String);

impl ControlToken {
    /// The token as sent over the wire (base64url, no padding).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ControlToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControlToken([REDACTED])")
    }
}

impl std::fmt::Display for ControlToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Displaying the token is as sensitive as Debug-ing it.
        f.write_str("[REDACTED]")
    }
}

/// Generate a fresh control token from the operating system's CSPRNG.
/// 256 bits of entropy; tokens are never derived from time, uuids, or counters.
pub fn generate_control_token() -> ControlToken {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    ControlToken(URL_SAFE_NO_PAD.encode(bytes))
}

/// SHA-256 hash of a control token. This is the **only** form the runtime
/// stores; verifying a presented token hashes it and compares digests.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretTokenHash([u8; 32]);

impl SecretTokenHash {
    pub fn from_token(token: &ControlToken) -> Self {
        Self(Sha256::digest(token.as_str().as_bytes()).into())
    }

    /// Constant-time comparison: the loop never exits early on a mismatch.
    pub fn verify(&self, presented: &str) -> bool {
        let digest = Sha256::digest(presented.as_bytes());
        let a = digest.as_slice();
        let b = &self.0;
        if a.len() != b.len() {
            return false;
        }
        let mut acc = 0u8;
        for i in 0..a.len() {
            acc |= a[i] ^ b[i];
        }
        acc == 0
    }

    /// Hex form for serialization (not currently persisted; kept for clarity).
    #[allow(dead_code)]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl std::fmt::Debug for SecretTokenHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretTokenHash([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_256_bits_of_randomness() {
        let a = generate_control_token();
        let b = generate_control_token();
        // 32 raw bytes → 43 base64url chars without padding.
        assert_eq!(a.as_str().len(), 43);
        assert_ne!(a, b, "two tokens must never collide");
    }

    #[test]
    fn hash_verifies_correct_token_and_rejects_others() {
        let token = generate_control_token();
        let hash = SecretTokenHash::from_token(&token);
        assert!(hash.verify(token.as_str()));
        assert!(!hash.verify("wrong-token"));
        assert!(!hash.verify(""));
        assert!(!hash.verify(&token.as_str()[..42]));
    }

    #[test]
    fn hash_does_not_contain_the_token() {
        let token = generate_control_token();
        let hash = SecretTokenHash::from_token(&token);
        assert!(!hash.to_hex().contains(&token.as_str()[..8].to_string()));
    }

    #[test]
    fn debug_redacts_token_and_hash() {
        let token = generate_control_token();
        let hash = SecretTokenHash::from_token(&token);
        let t = format!("{token:?} {token}");
        let h = format!("{hash:?}");
        assert_eq!(t, "ControlToken([REDACTED]) [REDACTED]");
        assert_eq!(h, "SecretTokenHash([REDACTED])");
        assert!(!t.contains(token.as_str()));
        assert!(!h.contains(&hash.to_hex()));
    }

    #[test]
    fn protocol_version_is_two() {
        assert_eq!(PROTOCOL_VERSION, 2);
    }
}
