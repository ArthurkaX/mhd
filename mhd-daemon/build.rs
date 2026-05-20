fn main() {
    let _ = embed_resource::compile("../icons/mhd.rc", embed_resource::NONE);

    // Ensure cargo re-runs the build script when the icon source changes
    println!("cargo:rerun-if-changed=../icons/mHD_256.png");
    println!("cargo:rerun-if-changed=../icons/mhd.ico");
    println!("cargo:rerun-if-changed=../icons/mhd.rc");
}
