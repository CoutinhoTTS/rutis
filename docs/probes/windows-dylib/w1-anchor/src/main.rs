use aimux_core as _;
use aimux_providers as _;
use rutis_agent as _;
use rutis_sdk as _;

fn main() {
    println!("sdk id {}", rutis_sdk::loaded_sdk_id());
}
