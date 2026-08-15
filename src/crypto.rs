//! Keyed digests used for consistency seeds, deterministic subset sampling,
//! and the `random_hash` generator.
//!
//! SHA-256 and HMAC come from the RustCrypto crates; what lives here is the
//! framing convention the rest of altonic relies on. Published test vectors
//! are kept below as a guard against a dependency bump changing behaviour.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    // HMAC accepts keys of any length, so this cannot fail.
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// HMAC over a list of parts with unambiguous length-prefixed framing, so
/// ("ab","c") never collides with ("a","bc").
pub fn hmac_parts(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(&(p.len() as u64).to_be_bytes());
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

pub fn to_hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// First 8 bytes of a digest as a u64 (big-endian) — the seed shape used by the RNG.
pub fn digest_to_u64(d: &[u8; 32]) -> u64 {
    u64::from_be_bytes(d[..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_vectors() {
        // RFC 4231 test case 1
        let key = [0x0bu8; 20];
        assert_eq!(
            to_hex(&hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // RFC 4231 test case 2
        assert_eq!(
            to_hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 4231 test case 6 (key longer than block size)
        let key = [0xaau8; 131];
        assert_eq!(
            to_hex(&hmac_sha256(&key, b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn framing_avoids_collisions() {
        let k = b"key";
        assert_ne!(hmac_parts(k, &[b"ab", b"c"]), hmac_parts(k, &[b"a", b"bc"]));
    }

    /// `hmac_parts` must stay byte-identical to HMAC over the concatenated
    /// length-prefixed framing: every consistency seed in the tool depends on
    /// it, so a change here silently re-masks every existing dataset.
    #[test]
    fn framing_is_stable() {
        assert_eq!(
            to_hex(&hmac_parts(b"secret", &[b"link", b"person", b"row-1"])),
            "22679433a3ce8484c57c8e70bf2ef50a31560f65aa1e86c6434dc11cfe7f781e"
        );
    }
}
