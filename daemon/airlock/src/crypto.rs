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
