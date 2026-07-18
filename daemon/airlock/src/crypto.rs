use anyhow::Result;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

pub fn hmac_sha256_hex(key: &[u8], bytes: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(bytes);
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time verify: does `provided_hex` match HMAC-SHA256(key, bytes)?
pub fn hmac_sha256_verify(key: &[u8], bytes: &[u8], provided_hex: &str) -> bool {
    let expected = match hex::decode(provided_hex) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(bytes);
    mac.verify_slice(&expected).is_ok()
}

/// Standard-alphabet base64 (RFC 4648) decode. Tolerates no whitespace/
/// padding errors (envelope producers are the daemon itself).
pub fn base64_decode(input: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let bytes: Vec<u8> = input.bytes().filter(|&b| b != b'\n' && b != b'\r').collect();
    if bytes.iter().any(|&b| b != b'=' && val(b).is_none()) {
        anyhow::bail!("invalid base64 character");
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in &bytes {
        if b == b'=' {
            break;
        }
        buf = (buf << 6) | val(b).ok_or_else(|| anyhow::anyhow!("bad b64"))? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// Standard-alphabet base64 (RFC 4648) encode. Used for the envelope-v2
/// `bodyB64` field so the verifier HMACs the exact bytes the signer did,
/// with no cross-language JSON canonicalization assumptions.
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut chunks = input.chunks_exact(3);
    for c in &mut chunks {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => unreachable!(),
    }
    out
}
