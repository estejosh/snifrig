fn main() {
    embed_resource::compile("../assets/snifrig.rc", embed_resource::NONE).manifest_optional().unwrap();
    for f in ["snifrig.exe", "snifrigd.exe", "snifrig-tray.exe", "snifrig-fix.exe", "snifrig-app.exe", "snifrig-mcp.exe"] {
        println!("cargo:rerun-if-changed=../target/release/{}", f);
    }
}
