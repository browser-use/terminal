//! RFC 6238 TOTP (SHA1, configurable digits/period) plus base32 seed decoding.
//!
//! The live code typed into a page is generated **in Python** (`_totp_now` in
//! `browser_script_helpers.py`) so it's fresh at fill time. This Rust copy is
//! the reference used to (a) validate a base32 seed at `secrets set --totp` time
//! and (b) pin the algorithm against RFC 6238 test vectors so the Python side
//! can't silently drift.

use sha1::{Digest, Sha1};

const SHA1_BLOCK: usize = 64;

/// Decode an RFC 4648 base32 string (case-insensitive, padding/whitespace
/// ignored) into bytes. Returns `None` on an invalid character.
pub fn base32_decode(input: &str) -> Option<Vec<u8>> {
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::new();
    for ch in input.chars() {
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32,
            '2'..='7' => ch as u32 - '2' as u32 + 26,
            '=' => continue,
            c if c.is_whitespace() => continue,
            _ => return None,
        };
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// Validate that a string is a usable base32 TOTP seed: decodes cleanly and
/// yields at least 10 bytes (an 80-bit key, the practical minimum). Returns the
/// decoded key bytes on success.
pub fn validate_totp_seed(seed: &str) -> Result<Vec<u8>, &'static str> {
    let trimmed = seed.trim();
    if trimmed.is_empty() {
        return Err("empty TOTP seed");
    }
    let key = base32_decode(trimmed).ok_or("TOTP seed is not valid base32")?;
    if key.len() < 10 {
        return Err("TOTP seed decodes to fewer than 10 bytes");
    }
    Ok(key)
}

fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    // Shorten an over-length key by hashing it first.
    let mut block = [0u8; SHA1_BLOCK];
    if key.len() > SHA1_BLOCK {
        let mut hasher = Sha1::new();
        hasher.update(key);
        let digest = hasher.finalize();
        block[..digest.len()].copy_from_slice(&digest);
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; SHA1_BLOCK];
    let mut opad = [0x5cu8; SHA1_BLOCK];
    for i in 0..SHA1_BLOCK {
        ipad[i] ^= block[i];
        opad[i] ^= block[i];
    }

    let mut inner = Sha1::new();
    inner.update(ipad);
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha1::new();
    outer.update(opad);
    outer.update(inner_digest);
    let result = outer.finalize();

    let mut out = [0u8; 20];
    out.copy_from_slice(&result);
    out
}

/// HOTP (RFC 4226) for a counter, producing a zero-padded `digits`-length code.
pub fn hotp(key: &[u8], counter: u64, digits: u32) -> String {
    let mac = hmac_sha1(key, &counter.to_be_bytes());
    let offset = (mac[19] & 0x0f) as usize;
    let bin = ((u32::from(mac[offset]) & 0x7f) << 24)
        | (u32::from(mac[offset + 1]) << 16)
        | (u32::from(mac[offset + 2]) << 8)
        | u32::from(mac[offset + 3]);
    let modulo = 10u32.pow(digits);
    let code = bin % modulo;
    format!("{code:0width$}", width = digits as usize)
}

/// TOTP (RFC 6238) for a given unix timestamp, period, and digit count.
pub fn totp_at(key: &[u8], unix_seconds: u64, period: u64, digits: u32) -> String {
    let counter = unix_seconds / period.max(1);
    hotp(key, counter, digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_decodes() {
        // 16 'A's are 80 zero bits → 10 zero bytes (low-entropy fixture).
        assert_eq!(base32_decode("AAAAAAAAAAAAAAAA").unwrap(), vec![0u8; 10]);
    }

    #[test]
    fn base32_is_case_and_whitespace_insensitive() {
        let a = base32_decode("ABABABABABABABAB").unwrap();
        let b = base32_decode("abab abab abab abab").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn base32_rejects_invalid() {
        assert!(base32_decode("0189!").is_none()); // 0,1,8,9,! are not base32
    }

    #[test]
    fn rfc6238_sha1_vectors() {
        // RFC 6238 Appendix B, SHA1, secret "12345678901234567890", 8 digits.
        let key = b"12345678901234567890";
        let cases = [
            (59u64, "94287082"),
            (1111111109, "07081804"),
            (1111111111, "14050471"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
            (20000000000, "65353130"),
        ];
        for (t, expected) in cases {
            assert_eq!(totp_at(key, t, 30, 8), expected, "T={t}");
        }
    }

    #[test]
    fn six_digit_is_low_order_of_eight() {
        let key = b"12345678901234567890";
        assert_eq!(totp_at(key, 59, 30, 6), "287082");
    }

    #[test]
    fn validate_seed() {
        assert!(validate_totp_seed("AAAAAAAAAAAAAAAA").is_ok()); // 16 chars → 10 bytes
        assert!(validate_totp_seed("  ABABABABABABABAB  ").is_ok());
        assert!(validate_totp_seed("").is_err());
        assert!(validate_totp_seed("not!base32").is_err());
        assert!(validate_totp_seed("AAAA").is_err()); // decodes to <10 bytes
    }
}
