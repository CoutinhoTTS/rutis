fn main() {
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() != Ok("unix") {
        return;
    }
    rutis_interop::build::from_manifest().expect("generate Cordis bindings");
}
