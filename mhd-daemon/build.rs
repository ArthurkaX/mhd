fn main() {
    embed_resource::compile("../icons/mhd.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("failed to embed icon resource");

    // Copy mhd.ico next to the binary so the tray can load it directly
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let profile_dir = out_dir.ancestors().nth(3).unwrap(); // …/target/debug/
    let ico_src = std::path::Path::new("../icons/mhd.ico");
    let ico_dst = profile_dir.join("mhd.ico");
    let _ = std::fs::copy(ico_src, &ico_dst);

    println!("cargo:rerun-if-changed=../icons/mHD_256.png");
    println!("cargo:rerun-if-changed=../icons/mhd.ico");
    println!("cargo:rerun-if-changed=../icons/mhd.rc");
}
