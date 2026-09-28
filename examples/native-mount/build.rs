fn main() {
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() != Ok("unix") {
        return;
    }
    rutis_interop::build::cordis_plugin(
        "../../interop/node/test/fixtures/counter.ts",
        "../../interop/node",
    )
    .expect("generate Cordis bindings during the normal Cargo build");
}
