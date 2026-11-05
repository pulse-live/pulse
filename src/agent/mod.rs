//! Autonomous executor: activate complete plans, schedule locally, and retain observations.
//! The control actor cannot increase local limits or execute arbitrary commands.

mod control;
mod execution;
mod lifecycle;
mod output;

use crate::{
    config::{Config, Mode},
    governor::Governor,
    observation::{Health, Observation, Phase, unix_ms},
    probe::Engine,
    protocol::{Check, Deployment, Plan},
    scheduler::Scheduler,
    storage::{Identity, Store},
    transport::Control,
};
use anyhow::{Context, Result, ensure};
use control::Update;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

struct Agent {
    config: Arc<Config>,
    store: Store,
    governor: Governor,
    engine: Engine,
    public_engine: Engine,
    control: Option<Control>,
    identity: Option<watch::Receiver<Identity>>,
    plan: Option<Plan>,
    scheduler: Scheduler,
    lease: Option<Instant>,
    generation: CancellationToken,
    community_stop: CancellationToken,
    community_enabled: bool,
    jobs: JoinSet<Observation>,
    active: HashSet<String>,
    live_tokens: HashMap<String, (String, CancellationToken)>,
    health: Health,
    output: Option<mpsc::Sender<Observation>>,
    health_tx: watch::Sender<Health>,
    revision: watch::Sender<u64>,
    updates: mpsc::Receiver<Update>,
}

/// Run a validated standalone or managed executor until local cancellation.
/// # Errors
/// Returns configuration, secure-state, persistence, worker, or output failures.
pub async fn run(config: Config, shutdown: CancellationToken) -> Result<Health> {
    config.validate()?;
    let managed = config.agent.mode == Mode::Managed;
    ensure!(
        managed || (!config.probes.is_empty() && config.output.stdout),
        "standalone execution requires HTTP probes and explicit output.stdout=true"
    );
    let store = Store::open(&config.agent.state_dir, &config.limits).await?;
    let _lock = store.lock()?;
    let recovered_drops = store.recover().await?;
    let (mut agent, output, cloud) = Agent::initialize(config, store, shutdown.clone()).await?;
    agent.health.dropped = agent.health.dropped.saturating_add(recovered_drops);
    let result = agent.run_loop(&shutdown).await;
    shutdown.cancel();
    agent.health.phase = Phase::Stopping;
    agent.generation.cancel();
    agent.community_stop.cancel();
    let drained = agent.drain().await;
    agent.health.active_probes = 0;
    agent.health.phase = Phase::Stopped;
    drop(agent.output);
    let output_result = match output {
        Some(output) => output
            .await
            .context("local output task failed")
            .and_then(|result| result),
        None => Ok(0),
    };
    if let Ok(dropped) = &output_result {
        agent.health.dropped = agent.health.dropped.saturating_add(*dropped);
    }
    let cloud_result = match cloud {
        Some(cloud) => cloud
            .await
            .context("control worker failed")
            .and_then(|result| result),
        None => Ok(()),
    };
    let checkpoint = agent.store.put("health", &agent.health).await;
    result?;
    drained?;
    output_result?;
    cloud_result?;
    checkpoint?;
    Ok(agent.health)
}

impl Agent {
    async fn run_loop(&mut self, shutdown: &CancellationToken) -> Result<()> {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while !shutdown.is_cancelled() {
            self.expire();
            self.schedule_due()?;
            let mut deadline = self
                .scheduler
                .next()
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
            if let Some(lease) = self.lease {
                deadline = deadline.min(lease);
            }
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                Some(result) = self.jobs.join_next() => {
                    let observation = result.context("probe task failed")?;
                    self.record(observation).await?;
                }
                Some(update) = self.updates.recv() => self.update(update).await,
                _ = tick.tick() => self.checkpoint().await?,
                () = tokio::time::sleep_until(deadline) => {},
            }
        }
        Ok(())
    }

    async fn update(&mut self, update: Update) {
        match update {
            Update::Plan { plan, applied } => {
                let result = self
                    .activate(plan)
                    .await
                    .map(|()| self.health.revision)
                    .map_err(|error| error.to_string());
                if result.is_err() {
                    tracing::warn!("plan rejected; last accepted revision retained");
                }
                let _ = applied.send(result);
            }
            Update::Work(command) => {
                if self.live(command).await.is_err() {
                    self.health.skipped = self.health.skipped.saturating_add(1);
                }
            }
        }
    }

    async fn activate(&mut self, plan: Plan) -> Result<()> {
        if let Some(previous) = &self.plan {
            plan.follows(previous)?;
        }
        if let Some(identity) = &self.identity
            && identity.borrow().credentials.deployment == Deployment::Community
        {
            ensure!(
                plan.assignments
                    .iter()
                    .all(|assignment| assignment.workload == crate::protocol::Workload::Community),
                "community identity cannot accept private routine assignments"
            );
        }
        let checks = plan.compile(&self.config, unix_ms())?;
        if self.plan.as_ref() == Some(&plan) {
            return Ok(());
        }
        let now = Instant::now();
        let wall = unix_ms();
        let scheduler = Scheduler::new(checks, plan.schedule_epoch_unix_ms, wall, now)?;
        self.store.put("plan", &plan).await?;
        self.generation.cancel();
        self.generation = CancellationToken::new();
        self.community_stop.cancel();
        self.community_stop = self.generation.child_token();
        self.lease =
            Some(now + Duration::from_millis(plan.valid_until_unix_ms.saturating_sub(wall)));
        self.scheduler = scheduler;
        self.health.revision = plan.revision;
        self.health.plan_expired = false;
        self.health.phase = Phase::Active;
        self.revision.send_replace(plan.revision);
        self.plan = Some(plan);
        Ok(())
    }

    fn expire(&mut self) {
        let expired = self.lease.is_some_and(|lease| Instant::now() >= lease)
            || (self.config.agent.mode == Mode::Managed
                && self
                    .plan
                    .as_ref()
                    .is_some_and(|plan| unix_ms() >= plan.valid_until_unix_ms));
        if expired && !self.health.plan_expired {
            self.generation.cancel();
            self.scheduler = Scheduler::default();
            self.lease = None;
            self.health.plan_expired = true;
            self.health.phase = Phase::LeaseExpired;
        }
    }

    async fn checkpoint(&mut self) -> Result<()> {
        let enabled = self
            .store
            .get::<bool>("community_enabled")
            .await?
            .unwrap_or(self.config.community.enabled);
        if self.community_enabled && !enabled {
            self.community_stop.cancel();
        }
        if !self.community_enabled && enabled {
            self.community_stop = self.generation.child_token();
        }
        self.community_enabled = enabled;
        if self.config.agent.mode == Mode::Managed && !self.health.plan_expired {
            self.health.phase = if self.plan.is_none() {
                Phase::WaitingForPlan
            } else if self
                .store
                .get::<bool>("control_healthy")
                .await?
                .unwrap_or(false)
            {
                Phase::Active
            } else {
                Phase::Offline
            };
        }
        self.health.active_probes = self.jobs.len();
        self.health_tx.send_replace(self.health.clone());
        self.store.put("health", &self.health).await
    }
}

fn local_checks(config: &Config) -> Result<Vec<Check>> {
    config
        .probes
        .iter()
        .map(|probe| {
            ensure!(
                probe.kind == crate::config::ProbeKind::Http,
                "only HTTP/HTTPS execution is implemented"
            );
            Ok(Check {
                revision: 0,
                assignment: crate::protocol::Assignment {
                    id: probe.id.clone(),
                    monitor_id: probe.id.clone(),
                    recipe_id: probe.id.clone(),
                    target: probe.target.clone(),
                    phase_offset_ms: probe.phase_offset_ms.unwrap_or_else(|| {
                        crate::scheduler::local_phase(&probe.id, probe.interval_ms, probe.jitter_ms)
                    }),
                    jitter_ms: probe.jitter_ms,
                    workload: crate::protocol::Workload::Routine,
                },
                recipe: crate::protocol::Recipe {
                    id: probe.id.clone(),
                    kind: probe.kind,
                    interval_ms: probe.interval_ms,
                    timeout_ms: probe.timeout_ms,
                    http: probe.http.clone(),
                },
            })
        })
        .collect()
}
