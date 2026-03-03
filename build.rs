fn main() {
    // Get the absolute path to the lib directory
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let lib_path = std::path::Path::new(&manifest_dir).join("lib");

    // Tell cargo to link the lighter-go signer library
    println!("cargo:rustc-link-search=native={}", lib_path.display());
    println!("cargo:rustc-link-lib=dylib=lighter-signer-linux-amd64");

    // Rerun if the library changes
    println!("cargo:rerun-if-changed=lib/lighter-signer-linux-amd64.so");
}
