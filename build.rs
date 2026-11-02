fn main() {
    // nng-sys (via kicad-ipc-rs) uses IsValidSecurityDescriptor from advapi32
    // on Windows but does not emit the link directive itself.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=advapi32");
    }
}
