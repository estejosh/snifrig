// Embeds the snifrig icon (assets/snifrig.ico) in every program, so Explorer, the taskbar,
// the Start menu and the tray show the hound instead of a generic program icon.
fn main() {
    println!("cargo:rerun-if-changed=assets/snifrig.rc");
    println!("cargo:rerun-if-changed=assets/snifrig.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/snifrig.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
}
