fn main() {
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() != Ok("unix") {
        return;
    }
    // Cordis plugins mounted by this crate are listed in Cargo.toml.
    rutis_bridge::cordis::build::from_manifest()
        .expect("generate Cordis bindings during the normal Cargo build");
    rutis_bridge::cordis::build::rutis_plugin(
        "src/lib.rs",
        "native_mount_example",
        "../../node/rutis-runtime",
    )
    .expect("generate rutis bindings during the normal Cargo build");
}
