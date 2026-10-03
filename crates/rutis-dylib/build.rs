use std::process::Command;

fn main() {
    // The example hosts run from target/<profile>/examples against the SDK in
    // target/<profile> and the toolchain's libstd. On macOS they get run
    // paths, so they also work where DYLD_LIBRARY_PATH is ignored (a host
    // signed with the hardened runtime). Linux test scripts use
    // LD_LIBRARY_PATH. Examples are test tools; nothing here ships.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let target = std::env::var("TARGET").unwrap();
    let output = Command::new(rustc)
        .args(["--print", "target-libdir", "--target", &target])
        .output()
        .expect("rustc --print target-libdir");
    let libdir = String::from_utf8(output.stdout).unwrap();
    println!("cargo:rustc-link-arg-examples=-Wl,-rpath,@loader_path/..");
    println!("cargo:rustc-link-arg-examples=-Wl,-rpath,{}", libdir.trim());
}
