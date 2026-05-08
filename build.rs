fn main() {
    // Compile C log shim for LV2 variadic printf forwarding
    cc::Build::new()
        .file("src/log_shim.c")
        .compile("log_shim");

    // Link suil when UI feature is enabled
    #[cfg(feature = "ui")]
    {
        if let Err(e) = pkg_config::probe_library("suil-0") {
            eprintln!("cargo:warning=suil-0 not found: {e}. UI support disabled at runtime.");
        }
    }
}
