use crate::{
    observation::{Health, unix_ms},
    storage::{Identity, Store},
};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{io::Write as _, path::Path};

pub(super) async fn status(path: &Path) -> Result<()> {
    ensure!(
        path.join("state.sqlite3").exists(),
        "no state exists at the selected directory"
    );
    let store = Store::metadata(path).await?;
    let identity = store.get::<Identity>("identity").await?;
    let health = store.get::<Health>("health").await?;
    let queued = store.queued().await?;
    let queued_bytes = store.queued_bytes().await?;
    writeln!(
        std::io::stdout().lock(),
        "{}",
        json!({"schema_version":1, "running":store.running()?,
        "agent_id":identity.map(|identity| identity.credentials.agent_id), "health":health,
        "queued_observations":queued,"queued_bytes":queued_bytes})
    )?;
    Ok(())
}

pub(super) async fn community(operation: super::CommunityOperation, path: &Path) -> Result<()> {
    ensure!(
        path.join("state.sqlite3").exists(),
        "community participation requires enrollment"
    );
    let store = Store::metadata(path).await?;
    let identity = store
        .get::<Identity>("identity")
        .await?
        .context("community participation requires enrollment")?;
    match operation {
        super::CommunityOperation::Enable => {
            identity.credentials.validate(unix_ms())?;
            store.put("community_enabled", &true).await?;
        }
        super::CommunityOperation::Disable => store.put("community_enabled", &false).await?,
        super::CommunityOperation::Status => {}
    }
    let enabled = store
        .get::<bool>("community_enabled")
        .await?
        .unwrap_or(false);
    writeln!(
        std::io::stdout().lock(),
        "{}",
        json!({"schema_version":1,"enabled":enabled})
    )?;
    Ok(())
}
