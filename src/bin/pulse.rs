use std::process::ExitCode;

use pulse::cli::{self, telemetry::Telemetry};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut telemetry = Telemetry::default();
    let result = match cli::start(&mut telemetry) {
        Ok(action) => action.execute().await,
        Err(error) => Err(error),
    };
    // Await blocking provider shutdown off the runtime thread, including on action failure.
    let shutdown = telemetry.shutdown().await;
    if let Err(error) = &result {
        eprintln!("error: {error:#}");
    }
    if let Err(error) = &shutdown {
        eprintln!("error: {error:#}");
    }
    if result.is_ok() && shutdown.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
