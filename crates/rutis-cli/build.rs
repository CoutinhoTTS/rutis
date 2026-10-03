fn main() {
    // The dylib host finds the SDK and libstd in its own bundle directory.
    // Set here rather than in RUSTFLAGS so plugins built alongside it get no
    // run path.
    if std::env::var_os("CARGO_FEATURE_DYLIB_PLUGINS").is_none() {
        return;
    }
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("linux") => println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN"),
        Ok("macos") => println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@loader_path"),
        _ => {}
    }
}
