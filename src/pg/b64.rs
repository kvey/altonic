//! Standard base64 (RFC 4648) with padding — used by SCRAM message fields.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(text.len());
    for c in text.bytes() {
        match c {
            b'A'..=b'Z' => vals.push(c - b'A'),
            b'a'..=b'z' => vals.push(c - b'a' + 26),
            b'0'..=b'9' => vals.push(c - b'0' + 52),
            b'+' => vals.push(62),
            b'/' => vals.push(63),
            b'=' => break,
            b'\r' | b'\n' => continue,
            _ => return None,
        }
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        match chunk.len() {
            4 => {
                let n = ((chunk[0] as u32) << 18)
                    | ((chunk[1] as u32) << 12)
                    | ((chunk[2] as u32) << 6)
                    | chunk[3] as u32;
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
                out.push(n as u8);
            }
            3 => {
                let n = ((chunk[0] as u32) << 18) | ((chunk[1] as u32) << 12) | ((chunk[2] as u32) << 6);
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
            }
            2 => {
                let n = ((chunk[0] as u32) << 18) | ((chunk[1] as u32) << 12);
                out.push((n >> 16) as u8);
            }
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), enc);
            assert_eq!(decode(enc).unwrap(), plain.as_bytes());
        }
        assert!(decode("!!!").is_none());
        // Round-trip binary
        let data: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&data)).unwrap(), data);
    }
}
