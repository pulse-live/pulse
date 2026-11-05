use std::{io::Write as _, path::Path};

use anyhow::Result;

use crate::config::Config;

pub(super) async fn execute(path: &Path) -> Result<()> {
    Config::load(path).await?;
    tracing::info!("configuration validated");
    writeln!(std::io::stdout().lock(), "Configuration is valid")?;
    Ok(())
}
