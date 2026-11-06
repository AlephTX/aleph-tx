fn main() {
    println!("cargo:rerun-if-changed=src/native/lighter-signer-linux-amd64.so");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64")
    {
        return;
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("Cargo sets manifest directory");
    let native_path = std::path::Path::new(&manifest_dir).join("src/native");
    println!("cargo:rustc-link-search=native={}", native_path.display());
    println!("cargo:rustc-link-lib=dylib:+verbatim=lighter-signer-linux-amd64.so");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", native_path.display());
}
