fn main() {
    // Tell Rust/Cargo to look in ./libs for libraries
    println!("cargo:rustc-link-search=native=libs");

    // Tell it to link with PCANBasic.lib
    println!("cargo:rustc-link-lib=PCANBasic");
}
