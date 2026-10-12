// Embeds the snifrig icon so the window, taskbar and Start menu show the hound.
fn main() {
    println!("cargo:rerun-if-changed=../assets/snifrig.rc");
    println!("cargo:rerun-if-changed=../assets/snifrig.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("../assets/snifrig.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
}
