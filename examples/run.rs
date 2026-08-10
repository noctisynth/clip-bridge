#[tokio::main]
async fn main() -> Result<(), clip_bridge::BridgeError> {
    clip_bridge::run().await
}
