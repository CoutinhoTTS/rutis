fn main() {
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() != Ok("unix") {
        return;
    }
    rutis_interop::build::cordis_plugin(
        "../../interop/node/test/fixtures/counter.ts",
        "../../interop/node",
    )
    .expect("generate Cordis bindings during the normal Cargo build");
    rutis_interop::build::rutis_plugin("src/lib.rs", "native_mount_example", "../../interop/node")
        .expect("generate rutis bindings during the normal Cargo build");
}
