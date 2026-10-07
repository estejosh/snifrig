//! Josh's license signing tool.
//!   snifrig-keygen new-keypair OUTDIR   writes OUTDIR\snifrig-fix-signing.secret (never commit it)
//!                                       and prints the public key as 64 hex chars and as a Rust array.
//!   snifrig-keygen sign --secret PATH --holder NAME --computers N --expires YYYY-MM-DD [--id ID]
//!                                       prints one key line (format in fixer/src/license.rs).
//! Refuses to write the secret inside a git working tree (walks up looking for .git).
use ed25519_compact::{KeyPair, SecretKey, Seed};
use snifrig_fix::json::esc;
use snifrig_fix::license::b64_encode;
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
    println!("public key rust: [{}]", pk.iter().map(|b| format!("0x{:02x}", b)).collect::<Vec<_>>().join(", "));
    Ok(())
}

fn sign(args: &[String]) -> Result<(), String> {
    let get = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone());
    let need = |name: &str| get(name).ok_or(format!("missing {}", name));
    let secret = std::fs::read_to_string(need("--secret")?).map_err(|e| format!("cannot read secret: {}", e))?;
    let sk = unhex(secret.trim()).and_then(|b| SecretKey::from_slice(&b).ok()).ok_or("secret file is not a valid key")?;
    let holder = need("--holder")?;
    let computers: u32 = need("--computers")?.parse().map_err(|_| "--computers must be a number")?;
    let expires = need("--expires")?;
    let b = expires.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' { return Err("--expires must look like YYYY-MM-DD".into()); }
    let id = get("--id").unwrap_or_else(|| format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)));
    let payload = format!(r#"{{"v":1,"holder":"{}","computers":{},"expires":"{}","id":"{}"}}"#, esc(&holder), computers, expires, esc(&id));
    let sig = sk.sign(payload.as_bytes(), None);
    println!("{}.{}", b64_encode(payload.as_bytes()), b64_encode(sig.as_ref()));
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(|s| s.as_str()) {
        Some("new-keypair") if args.len() == 2 => new_keypair(&args[1]),
        Some("sign") => sign(&args[1..]),
        _ => Err("usage: snifrig-keygen new-keypair OUTDIR | sign --secret PATH --holder NAME --computers N --expires YYYY-MM-DD [--id ID]".into()),
    };
    if let Err(e) = r {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}
