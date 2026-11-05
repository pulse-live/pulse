use crate::{agent, config::Config};
use anyhow::{Context, Result};
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub(super) async fn execute(path: &Path) -> Result<()> {
    let config = Config::load(path).await?;
    let stop = CancellationToken::new();
    let run = agent::run(config, stop.clone());
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => { result?; }
        signal = signal() => { stop.cancel(); run.await?; signal?; }
    }
    Ok(())
}

async fn signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("cannot install SIGTERM handler")?;
        tokio::select! { result = tokio::signal::ctrl_c() => result.context("cannot listen for SIGINT")?, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .context("cannot listen for shutdown")?;
    Ok(())
}
