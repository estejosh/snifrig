//! Offline license key check for fixing.
//! Dry run is free. Ask and auto modes need a valid key. No network, ever.
//!
//! Key file format (snifrig-fix.key, one line): base64url(payload) "." base64url(signature)
//!   payload is JSON: {"v":1,"holder":"..","computers":N,"expires":"YYYY-MM-DD","id":".."}
//!   signature = Ed25519 over the raw payload bytes, verified against PUBLIC_KEY.
//! PUBLIC_KEY is all zeros until Josh generates the production keypair with snifrig-keygen;
//! an all-zero key rejects every license with "no production signing key configured yet".
//! In debug builds only (cfg(debug_assertions)) env SNIFRIG_FIX_PUBKEY (64 hex chars) overrides
//! PUBLIC_KEY so tests can use a throwaway keypair. Release builds ignore it.
//! Expired keys are invalid. Today comes from SystemTime (civil-from-days, no chrono).
//! This is honest-user friction, not DRM.

use crate::json::{num_field, str_field};
use std::path::Path;

pub const PUBLIC_KEY: [u8; 32] = [0u8; 32];

const KEY_FILE: &str = "snifrig-fix.key";
const NO_KEY_NOTE: &str = "no production signing key configured yet";

#[derive(Clone, Debug, Default)]
pub struct LicenseStatus { pub licensed: bool, pub holder: String, pub computers: u32, pub expires: String, pub note: String }

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url, no padding.
pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// base64url, no padding. None on any bad character or length.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 4 == 1 { return None; }
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for ch in s.bytes() {
        let v = B64.iter().position(|&b| b == ch)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Today as YYYY-MM-DD (UTC).
fn today() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let z = (secs / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// The key to verify against: PUBLIC_KEY, or in debug builds the SNIFRIG_FIX_PUBKEY override.
fn active_key() -> [u8; 32] {
    #[cfg(debug_assertions)]
    if let Ok(h) = std::env::var("SNIFRIG_FIX_PUBKEY") {
        if h.len() == 64 && h.is_ascii() {
            let mut k = [0u8; 32];
            let ok = (0..32).all(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).map(|v| k[i] = v).is_ok());
            if ok { return k; }
        }
    }
    PUBLIC_KEY
}

fn bad(note: &str) -> LicenseStatus { LicenseStatus { note: note.to_string(), ..Default::default() } }

/// Reads dir/snifrig-fix.key and verifies it.
pub fn status(dir: &Path) -> LicenseStatus {
    match std::fs::read_to_string(dir.join(KEY_FILE)) {
        Ok(line) => verify(&line, &active_key()).unwrap_or_else(|e| bad(&e)),
        Err(_) => bad("no license key installed"),
    }
}

/// Verifies the key at `path`, and if valid copies it to dir/snifrig-fix.key. Ok(summary) or Err(why).
pub fn install(dir: &Path, path: &Path) -> Result<String, String> {
    let line = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let st = verify(&line, &active_key())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
    std::fs::write(dir.join(KEY_FILE), line.trim()).map_err(|e| format!("cannot write key file: {}", e))?;
    Ok(format!("licensed to {} for {} computer(s), expires {}", st.holder, st.computers, st.expires))
}

/// Verify one key line against a public key. Used by status/install and by tests.
pub fn verify(line: &str, pk: &[u8; 32]) -> Result<LicenseStatus, String> {
    if *pk == [0u8; 32] { return Err(NO_KEY_NOTE.to_string()); }
    let (p, s) = line.trim().split_once('.').ok_or("malformed license key")?;
    let payload = b64_decode(p).ok_or("malformed license key")?;
    let sig = b64_decode(s).ok_or("malformed license key")?;
    let pk = ed25519_compact::PublicKey::from_slice(pk).map_err(|_| "bad public key".to_string())?;
    let sig = ed25519_compact::Signature::from_slice(&sig).map_err(|_| "malformed license key")?;
    pk.verify(&payload, &sig).map_err(|_| "license signature is not valid".to_string())?;
    let json = String::from_utf8(payload).map_err(|_| "malformed license key")?;
    let expires = str_field(&json, "expires").filter(|e| valid_date(e)).ok_or("license has no valid expiry")?;
    if expires < today() { return Err(format!("license expired on {}", expires)); }
    Ok(LicenseStatus {
        licensed: true,
        holder: str_field(&json, "holder").unwrap_or_default(),
        computers: num_field(&json, "computers").unwrap_or(1.0) as u32,
        expires,
        note: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_compact::{KeyPair, Seed};

    fn kp() -> KeyPair { KeyPair::from_seed(Seed::new([7u8; 32])) }
    fn key(payload: &str) -> String {
        format!("{}.{}", b64_encode(payload.as_bytes()), b64_encode(kp().sk.sign(payload.as_bytes(), None).as_ref()))
    }
    fn pk() -> [u8; 32] { *kp().pk }
    const GOOD: &str = r#"{"v":1,"holder":"Test Holder","computers":2,"expires":"2099-12-31","id":"t1"}"#;

    #[test]
    fn valid_key_verifies() {
        let st = verify(&key(GOOD), &pk()).unwrap();
        assert!(st.licensed);
        assert_eq!((st.holder.as_str(), st.computers, st.expires.as_str()), ("Test Holder", 2, "2099-12-31"));
    }

    #[test]
    fn tampered_payload_fails() {
        let k = key(GOOD);
        let (_, sig) = k.split_once('.').unwrap();
        let forged = format!("{}.{}", b64_encode(GOOD.replace("\"computers\":2", "\"computers\":99").as_bytes()), sig);
        assert!(verify(&forged, &pk()).is_err());
    }

    #[test]
    fn expired_key_fails() {
        let k = key(&GOOD.replace("2099-12-31", "2020-01-01"));
        assert!(verify(&k, &pk()).unwrap_err().contains("expired"));
    }

    #[test]
    fn zero_public_key_rejects() {
        assert!(verify(&key(GOOD), &PUBLIC_KEY).unwrap_err().contains("no production signing key"));
    }

    #[test]
    fn base64url_round_trips() {
        for n in 0..20u8 {
            let d: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37).wrapping_add(250)).collect();
            let e = b64_encode(&d);
            assert!(!e.contains('=') && !e.contains('+') && !e.contains('/'));
            assert_eq!(b64_decode(&e).unwrap(), d);
        }
    }

    #[test]
    fn today_looks_like_a_date() { assert!(valid_date(&today())); }
}
