use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match clip_bridge::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("clip-bridge: {error}");
            ExitCode::FAILURE
        }
    }
}
