//! Random tokens, hashing and constant-time comparison.

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher as _, PasswordVerifier as _};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

/// A fresh 256-bit random value, base64url without padding (43 chars).
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system's random number generator failed");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// SHA-256 of `value`, base64url without padding. Tokens are stored only in
/// this form.
pub fn hash_token(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}

/// Constant-time string equality (the length is not hidden).
pub fn ct_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Whether `verifier` is a valid RFC 7636 code verifier whose S256
/// challenge is `challenge`.
pub fn pkce_s256_matches(verifier: &str, challenge: &str) -> bool {
    let valid = (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
    valid
        && ct_eq(
            &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            challenge,
        )
}

/// Whether `challenge` looks like an S256 challenge (43 base64url chars).
pub fn is_s256_challenge(challenge: &str) -> bool {
    challenge.len() == 43
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn argon2() -> Argon2<'static> {
    // Tests use cheap parameters: Argon2 is very slow in debug builds. The
    // parameters are stored in the hash, so verification follows them.
    #[cfg(test)]
    {
        let params = argon2::Params::new(1024, 1, 1, None).expect("valid test params");
        Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
    }
    #[cfg(not(test))]
    {
        // Argon2id, 19 MiB, 2 passes: the OWASP-recommended minimum.
        Argon2::default()
    }
}

/// Hash a password as an Argon2id PHC string with a random salt.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    argon2()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("could not hash the password: {e}"))
}

/// Check a password against a stored PHC string. Argon2's comparison is
/// constant-time; a malformed hash never matches.
pub fn verify_password(password: &str, phc: &str) -> bool {
    let Ok(hash) = PasswordHash::new(phc) else {
        return false;
    };
    argon2().verify_password(password.as_bytes(), &hash).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_and_hash_stably() {
        let a = random_token();
        assert_eq!(a.len(), 43);
        assert_ne!(a, random_token());
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_ne!(hash_token(&a), a);
    }

    #[test]
    fn pkce_s256_rfc7636_example() {
        // Appendix B of RFC 7636.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(is_s256_challenge(challenge));
        assert!(pkce_s256_matches(verifier, challenge));
        assert!(!pkce_s256_matches(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXl",
            challenge
        ));
        assert!(!pkce_s256_matches("short", challenge));
        assert!(!is_s256_challenge("plain-challenge"));
    }

    #[test]
    fn passwords_verify() {
        let phc = hash_password("correct horse battery").unwrap();
        assert!(phc.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery", &phc));
        assert!(!verify_password("correct horse batter", &phc));
        assert!(!verify_password("x", "not a hash"));
    }
}
