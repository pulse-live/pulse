use crate::observation::{Origin, Outcome};
use crate::{
    config::{Config, HttpPolicy, ProbeKind},
    governor::{Class, Governor},
    probe::Engine,
    protocol::{Assignment, Check, Recipe, Workload},
};
use anyhow::{Result, ensure};
use std::io::Write as _;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(super) async fn execute(protocol: ProbeKind, target: &str, allow_private: bool) -> Result<()> {
    ensure!(
        protocol == ProbeKind::Http,
        "this probe protocol is not implemented"
    );
    let config = Config::parse("[agent]\nmode = 'standalone'")?;
    let governor = Governor::new(
        config.limits.clone(),
        config.community.clone(),
        false,
        Instant::now(),
    )?;
    let _permit = governor
        .admit(Class::Urgent, Instant::now(), false)
        .map_err(|reason| anyhow::anyhow!("admission rejected: {reason:?}"))?;
    let engine = Engine::new(&config, governor, allow_private).await?;
    let check = Check {
        revision: 0,
        assignment: Assignment {
            id: "manual".into(),
            monitor_id: "manual".into(),
            recipe_id: "manual".into(),
            target: target.into(),
            phase_offset_ms: 0,
            jitter_ms: 0,
            workload: Workload::Routine,
        },
        recipe: Recipe {
            id: "manual".into(),
            kind: ProbeKind::Http,
            interval_ms: 5000,
            timeout_ms: 5000,
            http: HttpPolicy::default(),
        },
    };
    let mut observation = engine
        .execute(&check, None, &CancellationToken::new(), None)
        .await;
    observation.set_origin(Origin::Manual);
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::to_string(&observation)?
    )?;
    ensure!(
        observation.outcome() == Outcome::Pass,
        "HTTP probe {:?}",
        observation.outcome()
    );
    Ok(())
}
