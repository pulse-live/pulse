//! Startup binds identity, private state, and local policy before any work is admitted.

use super::{
    Agent, Arc, Config, Control, Deployment, Engine, Governor, Health, Identity, Mode, Plan,
    Scheduler, Store, local_checks,
};
use anyhow::{Context, Result, ensure};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

type Worker = JoinHandle<Result<()>>;
type OutputWorker = JoinHandle<Result<u64>>;

impl Agent {
    pub(super) async fn initialize(
        config: Config,
        store: Store,
        shutdown: CancellationToken,
    ) -> Result<(Self, Option<OutputWorker>, Option<Worker>)> {
        let config = Arc::new(config);
        let managed = config.agent.mode == Mode::Managed;
        let governor = Governor::new(
            config.limits.clone(),
            config.community.clone(),
            managed,
            Instant::now(),
        )?;
        let (updates, receiver) = mpsc::channel(8);
        let (revision, revision_rx) = watch::channel(0);
        let (health_tx, health_rx) = watch::channel(Health::default());
        let link = link(&config, &store).await?;
        let private = config.network.allow_private_networks
            && link
                .as_ref()
                .is_none_or(|(_, identity)| identity.credentials.deployment == Deployment::Private);
        let public_engine = Engine::new(&config, governor.clone(), false).await?;
        let engine = if private {
            Engine::new(&config, governor.clone(), true).await?
        } else {
            public_engine.clone()
        };
        let (control, identity_rx, session) = if let Some((control, identity)) = link {
            let (identity_tx, identity_rx) = watch::channel(identity);
            let session = super::control::Session {
                config: config.clone(),
                store: store.clone(),
                client: control.clone(),
                identity: identity_tx,
                revision: revision_rx,
                updates,
                health: health_rx,
                next_plan: Instant::now(),
                acknowledged: 0,
                next_work: Instant::now(),
                next_health: Instant::now(),
            };
            (Some(control), Some(identity_rx), Some(session))
        } else {
            (None, None, None)
        };
        let generation = CancellationToken::new();
        let community_stop = generation.child_token();
        let community_enabled = store
            .get::<bool>("community_enabled")
            .await?
            .unwrap_or(config.community.enabled);
        store.put("community_enabled", &community_enabled).await?;
        let mut agent = Self {
            config,
            store,
            governor,
            engine,
            public_engine,
            control,
            identity: identity_rx,
            plan: None,
            scheduler: Scheduler::default(),
            lease: None,
            generation,
            community_stop,
            community_enabled,
            jobs: tokio::task::JoinSet::new(),
            active: std::collections::HashSet::new(),
            live_tokens: std::collections::HashMap::new(),
            health: Health::default(),
            output: None,
            health_tx,
            revision,
            updates: receiver,
        };
        agent.restore().await?;
        agent.health_tx.send_replace(agent.health.clone());
        let (output, output_worker) = output(&agent.config, &shutdown);
        agent.output = output;
        let cloud = session.map(|session| tokio::spawn(session.run(shutdown)));
        Ok((agent, output_worker, cloud))
    }

    async fn restore(&mut self) -> Result<()> {
        self.health = self
            .store
            .get::<Health>("health")
            .await?
            .unwrap_or_default();
        self.health.active_probes = 0;
        self.health.peak_active_probes = 0;
        if self.config.agent.mode == Mode::Managed {
            if let Some(plan) = self.store.get::<Plan>("plan").await? {
                // Keep the revision even after lease expiry to prevent rollback or 304 renewal.
                if plan
                    .compile(&self.config, crate::observation::unix_ms())
                    .is_ok()
                {
                    self.activate(plan).await?;
                } else {
                    self.health.revision = plan.revision;
                    self.health.plan_expired = true;
                    self.health.phase = crate::observation::Phase::LeaseExpired;
                    self.revision.send_replace(plan.revision);
                    self.plan = Some(plan);
                }
            }
        } else {
            self.health.plan_expired = false;
            self.health.phase = crate::observation::Phase::Active;
            let now = crate::observation::unix_ms();
            self.scheduler = Scheduler::new(local_checks(&self.config)?, 0, now, Instant::now())?;
        }
        Ok(())
    }
}

fn output(
    config: &Config,
    shutdown: &CancellationToken,
) -> (
    Option<mpsc::Sender<crate::observation::Observation>>,
    Option<OutputWorker>,
) {
    if !config.output.stdout {
        return (None, None);
    }
    let (sender, receiver) = mpsc::channel(config.limits.max_buffered_observations);
    let shutdown = shutdown.clone();
    let worker = tokio::spawn(async move {
        let result = super::output::run(receiver, shutdown.clone()).await;
        if result.is_err() {
            shutdown.cancel();
        }
        result
    });
    (Some(sender), Some(worker))
}

async fn link(config: &Config, store: &Store) -> Result<Option<(Control, Identity)>> {
    if config.agent.mode != Mode::Managed {
        return Ok(None);
    }
    let identity = store
        .get::<Identity>("identity")
        .await?
        .context("managed mode requires enrollment in this state directory")?;
    identity.credentials.validate(0)?;
    let client = Control::new(
        config
            .agent
            .control_plane_url
            .as_deref()
            .context("missing control plane")?,
        config.agent.ca_file.as_deref(),
    )
    .await?;
    ensure!(
        client.server() == identity.server,
        "enrollment identity belongs to a different control plane"
    );
    Ok(Some((client, identity)))
}
