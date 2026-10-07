//! Offline paid-use key check for snifrig-fix (UFL 3.7, Operational Scope: Paid).
//! No network, ever. snifrig-fix does not run at all without a valid key (see main.rs gate).
//!
//! Same key format as the UFL reference issuer (examples/snifrig/offline-key/keygen.py):
//!   key = base64url(payload) "." base64url(Ed25519 signature over the raw payload bytes)
//!   (padding optional on decode). payload = compact JSON with sorted keys:
//!   {"component":"snifrig-fix","issued_at":"YYYY-MM-DD","license":"UFL-3.7","licensee":"..",
//!    "machine":"<optional sha256 hex>","not_after":"YYYY-MM-DD","seats":N,"v":1}
//! Valid only if: signature verifies against PUBLIC_KEY; license == UFL-3.7; component == snifrig-fix;
//! issued_at <= today (UTC) <= not_after; the optional machine equals this machine's hash
//! (lowercase hex SHA-256 of HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid); and today is not earlier
//! than the date in data-dir fix-lastseen.txt (a clock set backwards is refused). On success the
//! last-seen file is advanced to max(today, stored).
//! PUBLIC_KEY is all zeros until the orchestrator embeds the production key; zeros reject every key.
//! Debug builds only: env SNIFRIG_FIX_PUBKEY (64 hex chars) overrides PUBLIC_KEY for tests.
//! This is honest-user friction, not DRM.

use crate::json::{esc, num_field, str_field};
use std::path::Path;

pub const PUBLIC_KEY: [u8; 32] = [0x24, 0x8d, 0x25, 0x15, 0x3a, 0x40, 0x33, 0xc0, 0x4d, 0x8e, 0x6c, 0x61, 0x3c, 0xc4, 0x3d, 0x91, 0xe7, 0xfe, 0xe7, 0x89, 0xa2, 0x51, 0x24, 0xa7, 0x25, 0xe2, 0x0f, 0x02, 0x65, 0xa5, 0xa7, 0x9e]; // production key, 2026-10-07; secret kept off git (move to Custodly)

pub const LICENSE_ID: &str = "UFL-3.7";
pub const COMPONENT: &str = "snifrig-fix";
pub const PRICING_URL: &str = "https://github.com/estejosh/snifrig/blob/main/PRICING.md";

const KEY_FILE: &str = "snifrig-fix.key";
const LASTSEEN_FILE: &str = "fix-lastseen.txt";
const ACCEPTED_FILE: &str = "fix-accepted.json";
const NO_KEY_NOTE: &str = "no production signing key configured yet";

#[derive(Clone, Debug, Default)]
pub struct LicenseStatus { pub licensed: bool, pub licensee: String, pub seats: u32, pub not_after: String, pub note: String }

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

/// base64url, padding optional. None on any bad character or length.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
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

/// The signed payload, byte for byte what keygen.py writes (sorted keys, compact).
pub fn build_payload(licensee: &str, seats: u32, issued_at: &str, not_after: &str, machine: Option<&str>) -> String {
    let m = match machine { Some(m) if !m.is_empty() => format!(r#""machine":"{}","#, esc(m)), _ => String::new() };
    format!(
        r#"{{"component":"{}","issued_at":"{}","license":"{}","licensee":"{}",{}"not_after":"{}","seats":{},"v":1}}"#,
        COMPONENT, esc(issued_at), LICENSE_ID, esc(licensee), m, esc(not_after), seats
    )
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

pub fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// Lowercase hex SHA-256 of the MachineGuid string, or None if it cannot be read.
pub fn machine_hash() -> Option<String> {
    machine_guid().map(|g| crate::sha256::sha256_hex(g.as_bytes()))
}

#[cfg(windows)]
fn machine_guid() -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    const RRF_SUBKEY_WOW64_64KEY: u32 = 0x0001_0000;
    let w = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (sub, val) = (w(r"SOFTWARE\Microsoft\Cryptography"), w("MachineGuid"));
    let mut buf = [0u16; 128];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, sub.as_ptr(), val.as_ptr(), RRF_RT_REG_SZ | RRF_SUBKEY_WOW64_64KEY,
            std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut size)
    };
    if rc != 0 { return None; }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let g = String::from_utf16(&buf[..n]).ok()?;
    if g.is_empty() { None } else { Some(g) }
}

#[cfg(not(windows))]
fn machine_guid() -> Option<String> { None }

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

fn read_lastseen(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(LASTSEEN_FILE)).ok().map(|s| s.trim().to_string()).filter(|s| valid_date(s))
}

/// Reads dir/snifrig-fix.key and verifies it against the real clock, machine and last-seen date.
/// On success advances fix-lastseen.txt.
pub fn check(dir: &Path) -> Result<LicenseStatus, String> {
    let line = std::fs::read_to_string(dir.join(KEY_FILE)).map_err(|_| "no license key installed".to_string())?;
    check_line(dir, &line)
}

fn check_line(dir: &Path, line: &str) -> Result<LicenseStatus, String> {
    let (today, last) = (today(), read_lastseen(dir));
    let st = verify(line, &active_key(), &today, machine_hash().as_deref(), last.as_deref())?;
    let newest = match &last { Some(l) if *l > today => l.clone(), _ => today };
    let _ = std::fs::write(dir.join(LASTSEEN_FILE), newest);
    Ok(st)
}

/// Like check, but never an Err: a status with `note` explaining why not.
pub fn status(dir: &Path) -> LicenseStatus {
    check(dir).unwrap_or_else(|e| LicenseStatus { note: e, ..Default::default() })
}

/// Verifies the key at `path`, and if valid copies it to dir/snifrig-fix.key. Ok(summary) or Err(why).
pub fn install(dir: &Path, path: &Path) -> Result<String, String> {
    let line = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
    let st = check_line(dir, &line)?;
    std::fs::write(dir.join(KEY_FILE), line.trim()).map_err(|e| format!("cannot write key file: {}", e))?;
    Ok(format!("licensed to {} for {} seat(s), valid until {}", st.licensee, st.seats, st.not_after))
}

/// Pure check of one key line. `today` and `last_seen` are YYYY-MM-DD; `machine` is this machine's hash.
pub fn verify(line: &str, pk: &[u8; 32], today: &str, machine: Option<&str>, last_seen: Option<&str>) -> Result<LicenseStatus, String> {
    if *pk == [0u8; 32] { return Err(NO_KEY_NOTE.to_string()); }
    let bad_sig = || "key is not signed by the Licensor".to_string();
    let (p, s) = line.trim().split_once('.').ok_or_else(bad_sig)?;
    let payload = b64_decode(p).ok_or_else(bad_sig)?;
    let sig = b64_decode(s).ok_or_else(bad_sig)?;
    let pk = ed25519_compact::PublicKey::from_slice(pk).map_err(|_| "bad public key".to_string())?;
    let sig = ed25519_compact::Signature::from_slice(&sig).map_err(|_| bad_sig())?;
    pk.verify(&payload, &sig).map_err(|_| bad_sig())?;
    let json = String::from_utf8(payload).map_err(|_| bad_sig())?;
    if str_field(&json, "license").as_deref() != Some(LICENSE_ID) || str_field(&json, "component").as_deref() != Some(COMPONENT) {
        return Err("key is for a different Component or license version".into());
    }
    if let Some(l) = last_seen {
        if today < l { return Err("the system clock is earlier than a date already seen on this machine".into()); }
    }
    if let Some(m) = str_field(&json, "machine").filter(|m| !m.is_empty()) {
        if machine != Some(m.as_str()) { return Err("key was issued for a different machine".into()); }
    }
    let issued = str_field(&json, "issued_at").filter(|d| valid_date(d));
    let until = str_field(&json, "not_after").filter(|d| valid_date(d));
    let (issued, until) = match (issued, until) { (Some(a), Some(b)) => (a, b), _ => return Err("license has no valid dates".into()) };
    if !(issued.as_str() <= today && today <= until.as_str()) {
        return Err(format!("key is valid {} to {}; get a fresh key from your account", issued, until));
    }
    Ok(LicenseStatus {
        licensed: true,
        licensee: str_field(&json, "licensee").unwrap_or_default(),
        seats: num_field(&json, "seats").unwrap_or(1.0) as u32,
        not_after: until,
        note: String::new(),
    })
}

/// True if dir/fix-accepted.json records acceptance of UFL-3.7 for snifrig-fix.
pub fn accepted(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(ACCEPTED_FILE)).map(|s|
        str_field(&s, "license").as_deref() == Some(LICENSE_ID) && str_field(&s, "component").as_deref() == Some(COMPONENT)).unwrap_or(false)
}

/// Records the Section 9 acceptance locally. Nothing is sent anywhere.
pub fn record_acceptance(dir: &Path) -> Result<(), String> {
    let unix = crate::now_unix() as u64;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
    std::fs::write(dir.join(ACCEPTED_FILE), format!(r#"{{"license":"{}","component":"{}","unix":{}}}"#, LICENSE_ID, COMPONENT, unix))
        .map_err(|e| format!("cannot write {}: {}", ACCEPTED_FILE, e))
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
    fn good() -> String { build_payload("Test Co", 2, "2026-01-01", "2026-12-31", None) }
    const TODAY: &str = "2026-10-07";

    #[test]
    fn payload_matches_keygen_py_order() {
        assert_eq!(good(), r#"{"component":"snifrig-fix","issued_at":"2026-01-01","license":"UFL-3.7","licensee":"Test Co","not_after":"2026-12-31","seats":2,"v":1}"#);
        assert!(build_payload("A", 1, "2026-01-01", "2026-02-01", Some("ab")).contains(r#""licensee":"A","machine":"ab","not_after""#));
    }

    #[test]
    fn valid_key_verifies() {
        let st = verify(&key(&good()), &pk(), TODAY, None, None).unwrap();
        assert!(st.licensed);
        assert_eq!((st.licensee.as_str(), st.seats, st.not_after.as_str()), ("Test Co", 2, "2026-12-31"));
        assert!(verify(&key(&good()), &pk(), "2026-01-01", None, None).is_ok());
        assert!(verify(&key(&good()), &pk(), "2026-12-31", None, Some("2026-12-30")).is_ok());
    }

    #[test]
    fn padded_base64_accepted() {
        let k = key(&good());
        let (p, s) = k.split_once('.').unwrap();
        let pad = |x: &str| format!("{}{}", x, "=".repeat((4 - x.len() % 4) % 4));
        assert!(verify(&format!("{}.{}", pad(p), pad(s)), &pk(), TODAY, None, None).is_ok());
    }

    #[test]
    fn wrong_component_fails() {
        let k = key(&good().replace("snifrig-fix", "snifrig"));
        assert!(verify(&k, &pk(), TODAY, None, None).unwrap_err().contains("different Component"));
    }

    #[test]
    fn wrong_license_version_fails() {
        let k = key(&good().replace("UFL-3.7", "UFL-3.6"));
        assert!(verify(&k, &pk(), TODAY, None, None).unwrap_err().contains("different Component or license version"));
    }

    #[test]
    fn expired_fails() {
        assert!(verify(&key(&good()), &pk(), "2027-01-01", None, None).unwrap_err().contains("get a fresh key"));
    }

    #[test]
    fn not_yet_valid_fails() {
        assert!(verify(&key(&good()), &pk(), "2025-12-31", None, None).unwrap_err().contains("2026-01-01"));
    }

    #[test]
    fn machine_mismatch_fails_and_match_passes() {
        let k = key(&build_payload("Test Co", 1, "2026-01-01", "2026-12-31", Some("aaaa")));
        assert!(verify(&k, &pk(), TODAY, Some("bbbb"), None).unwrap_err().contains("different machine"));
        assert!(verify(&k, &pk(), TODAY, None, None).unwrap_err().contains("different machine"));
        assert!(verify(&k, &pk(), TODAY, Some("aaaa"), None).is_ok());
    }

    #[test]
    fn clock_earlier_than_last_seen_fails() {
        let e = verify(&key(&good()), &pk(), TODAY, None, Some("2026-11-01")).unwrap_err();
        assert!(e.contains("system clock is earlier"));
    }

    #[test]
    fn tampered_payload_fails() {
        let k = key(&good());
        let (_, sig) = k.split_once('.').unwrap();
        let forged = format!("{}.{}", b64_encode(good().replace("\"seats\":2", "\"seats\":99").as_bytes()), sig);
        assert!(verify(&forged, &pk(), TODAY, None, None).unwrap_err().contains("not signed"));
    }

    #[test]
    fn wrong_signer_and_garbage_fail() {
        let other = KeyPair::from_seed(Seed::new([9u8; 32]));
        assert!(verify(&key(&good()), &*other.pk, TODAY, None, None).is_err());
        assert!(verify("garbage", &pk(), TODAY, None, None).is_err());
    }

    #[test]
    fn zero_public_key_rejects() {
        assert!(verify(&key(&good()), &[0u8; 32], TODAY, None, None).unwrap_err().contains("no production signing key"));
        assert_ne!(PUBLIC_KEY, [0u8; 32], "release builds must embed the production key");
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

    #[cfg(windows)]
    #[test]
    fn machine_hash_is_64_hex() {
        let h = machine_hash().unwrap();
        assert!(h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }
}
