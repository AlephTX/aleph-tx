fn main() {
    // Get the absolute path to the src/native directory
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let native_path = std::path::Path::new(&manifest_dir).join("src/native");

    // Tell cargo to link the lighter-go signer library
    println!("cargo:rustc-link-search=native={}", native_path.display());
    println!("cargo:rustc-link-lib=dylib=lighter-signer-linux-amd64");

    // Rerun if the library changes
    println!("cargo:rerun-if-changed=src/native/lighter-signer-linux-amd64.so");
}
