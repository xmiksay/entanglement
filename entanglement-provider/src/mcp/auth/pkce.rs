//! PKCE (RFC 7636) and CSRF-state generation for the MCP OAuth flow (ADR-0153).
//!
//! OAuth 2.1 makes PKCE mandatory for the authorization-code grant, and a public
//! client (which we are — no client secret survives on a user's disk in any
//! meaningful sense) has no other defence against an intercepted code. We only
//! ever offer `S256`; the spec's `plain` method is deliberately not implemented.
//!
//! Randomness comes from `uuid`'s v4 generator — the same crate `entanglement-core`
//! already uses for session ids, and a `getrandom`-backed CSPRNG underneath. Two
//! UUIDs give 32 bytes of entropy, which base64url-encodes to the 43-character
//! verifier RFC 7636 §4.1 asks for.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// A generated PKCE pair: the secret `verifier` kept in memory until the token
/// exchange, and the `challenge` sent on the authorization request.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// Generate a fresh `S256` pair.
    pub fn generate() -> Self {
        let verifier = random_token();
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Self {
            verifier,
            challenge,
        }
    }

    /// The challenge method we advertise. Always `S256` — `plain` is not offered.
    pub const fn method() -> &'static str {
        "S256"
    }
}

/// 32 bytes of CSPRNG entropy, base64url-encoded without padding (43 chars).
/// Used for the PKCE verifier and for the `state` parameter that binds the
/// redirect back to the request we started.
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_meets_rfc7636_length_and_charset() {
        let p = Pkce::generate();
        // RFC 7636 §4.1: 43..=128 characters from the unreserved set.
        assert!(
            (43..=128).contains(&p.verifier.len()),
            "verifier length {} out of range",
            p.verifier.len()
        );
        assert!(p
            .verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')));
    }

    #[test]
    fn challenge_is_the_base64url_sha256_of_the_verifier() {
        let p = Pkce::generate();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expected);
        // Base64url of a 32-byte digest, unpadded.
        assert_eq!(p.challenge.len(), 43);
        assert!(!p.challenge.contains('='));
        assert!(!p.challenge.contains('+') && !p.challenge.contains('/'));
    }

    #[test]
    fn known_vector_from_rfc7636_appendix_b() {
        // RFC 7636 Appendix B pins the verifier → challenge transformation.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn tokens_are_unique_across_calls() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
    }

    #[test]
    fn method_is_s256_only() {
        assert_eq!(Pkce::method(), "S256");
    }
}
