use std::{env, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");
    let web = root.join("web");
    println!("cargo:rustc-env=ORCH_WEB_DIR={}", web.display());
    for file in [
        "index.html",
        "app.css",
        "boot.js",
        "icons/logo.svg",
        "assets/orch_ui.js",
        "assets/orch_ui_bg.wasm",
        "assets/abi-manifest.json",
    ] {
        println!("cargo:rerun-if-changed={}", web.join(file).display());
    }
}
