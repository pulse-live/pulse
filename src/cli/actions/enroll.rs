use crate::{
    config::Limits,
    protocol::{Enrollment, Secret},
    storage::{Identity, Store},
    transport::Control,
};
use anyhow::{Result, ensure};
use std::{io::Write as _, path::Path};

pub(super) async fn execute(
    server: &str,
    token: Secret,
    path: &Path,
    ca: Option<&Path>,
) -> Result<()> {
    ensure!(
        !token.0.is_empty() && token.0.len() <= 4096,
        "invalid enrollment token"
    );
    let store = if path.join("state.sqlite3").exists() {
        Store::metadata(path).await?
    } else {
        Store::open(path, &Limits::default()).await?
    };
    let _lock = store.lock()?;
    let previous = store.get::<Identity>("identity").await?;
    let client = Control::new(server, ca).await?;
    if let Some(previous) = &previous {
        ensure!(
            previous.server == client.server(),
            "state directory belongs to a different control plane"
        );
    }
    let request = Enrollment {
        schema_version: 1,
        token,
        capabilities: vec![
            "http".into(),
            "plan-v1".into(),
            "long-poll".into(),
            "protobuf-observations-v1".into(),
        ],
    };
    let credentials = client.enroll(&request).await?;
    if let Some(previous) = previous {
        ensure!(
            previous.credentials.agent_id == credentials.agent_id,
            "recovery token must identify the existing agent; state was not changed"
        );
    }
    let id = credentials.agent_id.clone();
    store
        .put(
            "identity",
            &Identity {
                server: client.server().to_owned(),
                credentials,
            },
        )
        .await?;
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::json!({"agent_id":id,"enrolled":true})
    )?;
    Ok(())
}
