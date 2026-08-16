//! Webhook signature checking.
//!
//! A webhook endpoint is a public, unauthenticated URL that starts expensive
//! model runs. The signature check is the only thing standing between it and
//! anyone who knows the address, so the comparison is constant time and an
//! empty secret is refused rather than treated as "no check needed".

use gitai_core::error::{Error, Result};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Verifies `signature` (hex, optionally `sha256=` prefixed) over `body`.
pub fn verify_hmac_sha256(secret: &str, body: &[u8], signature: &str, forge: &str) -> Result<()> {
    if secret.is_empty() {
        return Err(Error::forge(
            forge,
            "webhook_secret is empty; refusing to accept unsigned deliveries",
        ));
    }
    let hex = signature
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or(signature.trim());
    let expected =
        decode_hex(hex).ok_or_else(|| Error::forge(forge, "signature header is not valid hex"))?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| Error::forge(forge, format!("bad webhook secret: {e}")))?;
    mac.update(body);
    mac.verify_slice(&expected)
        .map_err(|_| Error::forge(forge, "webhook signature mismatch"))
}

/// Compares a shared secret sent verbatim in a header.
///
/// GitLab does not sign its deliveries, it just echoes a token back, so this is
/// the whole of its authentication. The comparison is constant time and length
/// is folded into the result, so neither the value nor its size leaks through
/// timing.
pub fn verify_shared_secret(expected: &str, provided: &str, forge: &str) -> Result<()> {
    if expected.is_empty() {
        return Err(Error::forge(
            forge,
            "webhook_secret is empty; refusing to accept unauthenticated deliveries",
        ));
    }

    let a = expected.as_bytes();
    let b = provided.as_bytes();
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }

    if diff == 0 {
        Ok(())
    } else {
        Err(Error::forge(forge, "webhook token mismatch"))
    }
}

/// Hex string to bytes. `None` on odd length or a non-hex digit.
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_val(pair[0])?;
        let lo = hex_val(pair[1])?;
        out.push(hi << 4 | lo);
    }
    Some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference vector: HMAC-SHA256 of "hello" keyed with "topsecret".
    const BODY: &[u8] = b"hello";
    const SECRET: &str = "topsecret";

    fn good_signature() -> String {
        let mut mac = HmacSha256::new_from_slice(SECRET.as_bytes()).unwrap();
        mac.update(BODY);
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn accepts_a_correct_signature_with_and_without_prefix() {
        let sig = good_signature();
        verify_hmac_sha256(SECRET, BODY, &sig, "test").unwrap();
        verify_hmac_sha256(SECRET, BODY, &format!("sha256={sig}"), "test").unwrap();
    }

    #[test]
    fn rejects_a_tampered_body() {
        let sig = good_signature();
        assert!(verify_hmac_sha256(SECRET, b"hello!", &sig, "test").is_err());
    }

    #[test]
    fn rejects_the_wrong_secret() {
        let sig = good_signature();
        assert!(verify_hmac_sha256("other", BODY, &sig, "test").is_err());
    }

    #[test]
    fn an_empty_secret_is_a_configuration_error_not_a_pass() {
        let err = verify_hmac_sha256("", BODY, &good_signature(), "test")
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn malformed_hex_is_rejected() {
        assert!(verify_hmac_sha256(SECRET, BODY, "zz", "test").is_err());
        assert!(verify_hmac_sha256(SECRET, BODY, "abc", "test").is_err());
        assert!(verify_hmac_sha256(SECRET, BODY, "", "test").is_err());
    }

    #[test]
    fn shared_secrets_match_only_when_identical() {
        verify_shared_secret("s3cret", "s3cret", "gitlab").unwrap();
        assert!(verify_shared_secret("s3cret", "s3cres", "gitlab").is_err());
        assert!(verify_shared_secret("s3cret", "s3cret ", "gitlab").is_err());
        assert!(verify_shared_secret("s3cret", "", "gitlab").is_err());
        assert!(verify_shared_secret("s3cret", "s3cret-and-more", "gitlab").is_err());
    }

    #[test]
    fn an_empty_expected_secret_is_refused_not_matched() {
        let err = verify_shared_secret("", "", "gitlab")
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn hex_decoding_round_trips() {
        assert_eq!(decode_hex("00ff10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(decode_hex("00FF10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(decode_hex("0"), None);
    }
}
