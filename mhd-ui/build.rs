use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=../icons/mHD_32.png");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let src = manifest_dir.join("..").join("icons").join("mHD_32.png");
    let dst_dir = manifest_dir.join("..").join("target").join(profile);
    let dst = dst_dir.join("mHD_32.png");

    if let Err(err) = fs::create_dir_all(&dst_dir).and_then(|_| fs::copy(&src, &dst).map(|_| ())) {
        println!("cargo:warning=failed to copy tray icon to {}: {err}", dst.display());
    }
}
