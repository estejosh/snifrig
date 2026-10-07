//! Hard gate: the built snifrig-fix binary must not run without a valid key and a recorded acceptance.
use std::path::PathBuf;
use std::process::{Command, Output};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("snifrig-fix-gate-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(dir: &PathBuf, pubkey_hex: Option<&str>, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_snifrig-fix"));
    c.args(args).arg("--dir").arg(dir).stdin(std::process::Stdio::null());
    if let Some(h) = pubkey_hex { c.env("SNIFRIG_FIX_PUBKEY", h); }
    c.output().unwrap()
}

#[test]
fn no_key_every_gated_command_exits_2() {
    let dir = tmp("nokey");
    // `[]` is the watch loop: it must exit immediately, not loop.
    for args in [&[][..], &["once"], &["status"], &["pending"], &["mode", "auto"], &["approve", "x"], &["dismiss", "x"], &["bogus"]] {
        let o = run(&dir, None, args);
        assert_eq!(o.status.code(), Some(2), "{:?}", args);
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("snifrig-fix is the paid part of Snifrig and needs a valid key. Prices: https://github.com/estejosh/snifrig/blob/main/PRICING.md"), "{:?}: {}", args, err);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exempt_commands_work_without_key() {
    let dir = tmp("exempt");
    let id = run(&dir, None, &["machine-id"]);
    assert_eq!(id.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&id.stdout).trim().len(), 64);
    assert_eq!(run(&dir, None, &["license", "status"]).status.code(), Some(0));
    assert_eq!(run(&dir, None, &["--help"]).status.code(), Some(0));
    // wrong acceptance text is refused, nothing recorded
    let bad = run(&dir, None, &["license", "accept", "--accept-license", "yes"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(!dir.join("fix-accepted.json").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(debug_assertions)]
#[test]
fn valid_key_still_needs_acceptance_then_runs() {
    use ed25519_compact::{KeyPair, Seed};
    use snifrig_fix::license::{b64_encode, build_payload};
    let kp = KeyPair::from_seed(Seed::new([3u8; 32]));
    let hex: String = kp.pk.iter().map(|b| format!("{:02x}", b)).collect();
    let payload = build_payload("Gate Test", 1, "2020-01-01", "2099-12-31", None);
    let key = format!("{}.{}", b64_encode(payload.as_bytes()), b64_encode(kp.sk.sign(payload.as_bytes(), None).as_ref()));
    let dir = tmp("keyed");
    let keyfile = dir.join("test.key");
    std::fs::write(&keyfile, key).unwrap();

    let inst = run(&dir, Some(&hex), &["license", "install", keyfile.to_str().unwrap()]);
    assert_eq!(inst.status.code(), Some(0), "{}", String::from_utf8_lossy(&inst.stderr));
    assert!(dir.join("fix-lastseen.txt").exists());

    let before = run(&dir, Some(&hex), &["status"]);
    assert_eq!(before.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&before.stderr).contains("license accept"));

    let acc = run(&dir, Some(&hex), &["license", "accept", "--accept-license", "UFL-3.7 snifrig-fix"]);
    assert_eq!(acc.status.code(), Some(0));
    let rec = std::fs::read_to_string(dir.join("fix-accepted.json")).unwrap();
    assert!(rec.contains(r#""license":"UFL-3.7","component":"snifrig-fix","unix":"#));

    let after = run(&dir, Some(&hex), &["status"]);
    assert_eq!(after.status.code(), Some(0), "{}", String::from_utf8_lossy(&after.stderr));

    // a last-seen date in the future (clock rolled back) makes the same key fail again
    std::fs::write(dir.join("fix-lastseen.txt"), "2098-01-01").unwrap();
    let rolled = run(&dir, Some(&hex), &["status"]);
    assert_eq!(rolled.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rolled.stderr).contains("system clock is earlier"));
    let _ = std::fs::remove_dir_all(&dir);
}
