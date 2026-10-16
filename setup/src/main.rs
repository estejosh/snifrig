//! snifrig-setup.exe: one file that installs snifrig for this Windows user. No admin, no
//! service, no network. It unpacks the programs into %LOCALAPPDATA%\\snifrig-setup and runs
//! `snifrig install`, which shows the license and asks you to accept it, copies the programs to
//! %LOCALAPPDATA%\\snifrig\\bin, starts them, and adds snifrig to Settings > Apps for uninstalling.
use std::path::PathBuf;

const FILES: [(&str, &[u8]); 6] = [
    ("snifrig.exe", include_bytes!("../../target/release/snifrig.exe")),
    ("snifrigd.exe", include_bytes!("../../target/release/snifrigd.exe")),
    ("snifrig-tray.exe", include_bytes!("../../target/release/snifrig-tray.exe")),
    ("snifrig-fix.exe", include_bytes!("../../target/release/snifrig-fix.exe")),
    ("snifrig-app.exe", include_bytes!("../../target/release/snifrig-app.exe")),
    ("snifrig-mcp.exe", include_bytes!("../../target/release/snifrig-mcp.exe")),
];
const LICENSE: &[u8] = include_bytes!("../../LICENSE");

fn pause() { println!("\nPress Enter to close."); let _ = std::io::stdin().read_line(&mut String::new()); }

fn main() {
    println!("snifrig {} setup\n", env!("CARGO_PKG_VERSION"));
    let base = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("snifrig-setup");
    let bin = base.join(env!("CARGO_PKG_VERSION")).join("bin");
    if let Err(e) = std::fs::create_dir_all(&bin) { println!("Could not create {}: {}", bin.display(), e); pause(); std::process::exit(1); }
    // install looks for LICENSE two folders above the programs, as in the source tree.
    let _ = std::fs::write(base.join("LICENSE"), LICENSE);
    for (name, bytes) in FILES {
        if let Err(e) = std::fs::write(bin.join(name), bytes) { println!("Could not unpack {}: {}", name, e); pause(); std::process::exit(1); }
    }
    let ok = std::process::Command::new(bin.join("snifrig.exe")).arg("install").status().map(|s| s.success()).unwrap_or(false);
    if ok {
        println!("\nDone. Look for the snifrig icon next to the clock, and snifrig in the Start menu.");
        let app = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("snifrig").join("bin").join("snifrig-app.exe");
        if app.exists() { let _ = std::process::Command::new(app).spawn(); }
    } else {
        println!("\nSetup did not finish. Nothing was changed if you did not accept the license.");
    }
    pause();
}
