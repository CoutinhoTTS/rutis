#[cfg(unix)]
include!(concat!(env!("OUT_DIR"), "/rutis.rs"));

#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run().await?;
    Ok(())
}

#[cfg(not(unix))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("the native mount development example currently requires Unix".into())
}
