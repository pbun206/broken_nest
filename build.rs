fn main() {
    cc::Build::new()
        .file("src/log_shim.c")
        .compile("log_shim");

    if std::env::var("CARGO_FEATURE_UI").is_ok() {
        if let Err(e) = pkg_config::probe_library("suil-0") {
            eprintln!("cargo:warning=suil-0 not found: {e}. UI support disabled at runtime.");
        }
    }
}
