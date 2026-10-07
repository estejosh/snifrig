//! Josh's license signing tool. Issues the same keys as the UFL reference keygen.py.
//!   snifrig-keygen new-keypair OUTDIR   writes OUTDIR\snifrig-fix-signing.secret (never commit it)
//!                                       and prints the public key as hex, base64url (keygen.py style)
//!                                       and as a Rust [u8; 32] literal.
//!   snifrig-keygen sign --secret PATH --licensee NAME --seats N --issued YYYY-MM-DD
//!                       --not-after YYYY-MM-DD [--machine HASH]
//!                                       prints one key line (format in fixer/src/license.rs).
//! HASH is what `snifrig-fix machine-id` prints on the buyer's machine.
//! Refuses to write the secret inside a git working tree (walks up looking for .git).
use ed25519_compact::{KeyPair, SecretKey, Seed};
use snifrig_fix::license::{b64_encode, build_payload, valid_date};
use std::path::Path;

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{:02x}", x)).collect() }

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() { return None; }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

fn in_git_tree(dir: &Path) -> bool {
    let mut cur = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    loop {
        if cur.join(".git").exists() { return true; }
        if !cur.pop() { return false; }
    }
}

fn new_keypair(outdir: &str) -> Result<(), String> {
    let dir = Path::new(outdir);
    let mut existing = dir;
    while !existing.exists() {
        match existing.parent() { Some(p) if !p.as_os_str().is_empty() => existing = p, _ => { existing = Path::new("."); break; } }
    }
    if in_git_tree(existing) { return Err("refusing to write the secret inside a git working tree".into()); }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", outdir, e))?;
    let file = dir.join("snifrig-fix-signing.secret");
    if file.exists() { return Err(format!("{} already exists, not overwriting", file.display())); }
    let kp = KeyPair::from_seed(Seed::generate());
    std::fs::write(&file, hex(kp.sk.as_ref())).map_err(|e| format!("cannot write secret: {}", e))?;
    let pk: &[u8] = kp.pk.as_ref();
    println!("secret written to {} (keep it off git)", file.display());
    println!("public key hex: {}", hex(pk));
    println!("public key base64url: {}", b64_encode(pk));
    println!("public key rust: [{}]", pk.iter().map(|b| format!("0x{:02x}", b)).collect::<Vec<_>>().join(", "));
    Ok(())
}

fn sign(args: &[String]) -> Result<(), String> {
    let get = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone());
    let need = |name: &str| get(name).ok_or(format!("missing {}", name));
    let secret = std::fs::read_to_string(need("--secret")?).map_err(|e| format!("cannot read secret: {}", e))?;
    let sk = unhex(secret.trim()).and_then(|b| SecretKey::from_slice(&b).ok()).ok_or("secret file is not a valid key")?;
    let licensee = need("--licensee")?;
    let seats: u32 = need("--seats")?.parse().map_err(|_| "--seats must be a number")?;
    let (issued, not_after) = (need("--issued")?, need("--not-after")?);
    if !valid_date(&issued) { return Err("--issued must look like YYYY-MM-DD".into()); }
    if !valid_date(&not_after) { return Err("--not-after must look like YYYY-MM-DD".into()); }
    if not_after < issued { return Err("--not-after is before --issued".into()); }
    let machine = get("--machine");
    if let Some(m) = &machine {
        if m.len() != 64 || !m.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err("--machine must be 64 lowercase hex chars (from `snifrig-fix machine-id`)".into());
        }
    }
    let payload = build_payload(&licensee, seats, &issued, &not_after, machine.as_deref());
    let sig = sk.sign(payload.as_bytes(), None);
    println!("{}.{}", b64_encode(payload.as_bytes()), b64_encode(sig.as_ref()));
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(|s| s.as_str()) {
        Some("new-keypair") if args.len() == 2 => new_keypair(&args[1]),
        Some("sign") => sign(&args[1..]),
        _ => Err("usage: snifrig-keygen new-keypair OUTDIR | sign --secret PATH --licensee NAME --seats N --issued YYYY-MM-DD --not-after YYYY-MM-DD [--machine HASH]".into()),
    };
    if let Err(e) = r {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}
