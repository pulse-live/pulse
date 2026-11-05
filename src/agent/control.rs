//! A bounded control actor, independent of scheduler timing. Notifications only trigger reconciliation.

use crate::{
    config::Config,
    observation::{Health, unix_ms},
    protocol::{Command, Plan},
    storage::{Identity, Store},
    transport::{Backoff, Control},
};
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub(super) enum Update {
    Plan {
        plan: Plan,
        applied: oneshot::Sender<Result<u64, String>>,
    },
    Work(Command),
}

pub(super) struct Session {
    pub config: Arc<Config>,
    pub store: Store,
    pub client: Control,
    pub identity: watch::Sender<Identity>,
    pub revision: watch::Receiver<u64>,
    pub updates: mpsc::Sender<Update>,
    pub health: watch::Receiver<Health>,
    pub next_plan: Instant,
    pub acknowledged: u64,
    pub next_work: Instant,
    pub next_health: Instant,
}

impl Session {
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<()> {
        let (notify, mut notifications) = mpsc::channel(1);
        let poller = if self.config.agent.long_poll {
            let client = self.client.clone();
            let identity = self.identity.subscribe();
            let stop = shutdown.clone();
            Some(tokio::spawn(async move {
                long_poll(client, identity, notify, stop).await;
            }))
        } else {
            None
        };
        let mut backoff = Backoff::default();
        let mut delay = Duration::ZERO;
        let mut retrying = false;
        let mut next_cycle = Instant::now();
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(delay) => {},
                Some(()) = notifications.recv(), if !retrying => { self.next_plan = Instant::now(); self.next_work = Instant::now(); },
            }
            tokio::select! { () = shutdown.cancelled() => break, () = tokio::time::sleep_until(next_cycle) => {} }
            next_cycle = Instant::now() + Duration::from_secs(1);
            if notifications.try_recv().is_ok() {
                self.next_plan = Instant::now();
                self.next_work = Instant::now();
            }
            let result = tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                result = self.step() => result,
            };
            retrying = result.is_err();
            let healthy = result.is_ok();
            if self.control_health(healthy).await.is_err() {
                retrying = true;
                tracing::warn!("cannot persist control health");
            }
            delay = if retrying {
                tracing::warn!("control reconciliation failed; retaining accepted state");
                backoff.delay()
            } else {
                backoff.reset();
                Duration::from_secs(1)
            };
        }
        if let Some(poller) = poller {
            poller.await.context("control poller task failed")?;
        }
        Ok(())
    }

    async fn control_health(&self, healthy: bool) -> Result<()> {
        if self.store.get::<bool>("control_healthy").await? != Some(healthy) {
            self.store.put("control_healthy", &healthy).await?;
        }
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    async fn step(&mut self) -> Result<()> {
        let mut identity = self.identity.borrow().clone();
        if identity
            .credentials
            .expires_unix_ms
            .saturating_sub(unix_ms())
            <= 60_000
        {
            identity.credentials = self.client.renew(&identity.credentials).await?;
            self.store.put("identity", &identity).await?;
            self.identity.send_replace(identity.clone());
        }
        let mut failed = false;
        if Instant::now() >= self.next_plan {
            self.next_plan =
                Instant::now() + Duration::from_millis(self.config.agent.sync_interval_ms);
            if self.fetch(&identity).await.is_err() {
                failed = true;
                self.next_plan = Instant::now();
            }
        }
        let revision = *self.revision.borrow();
        if revision > 0 && revision != self.acknowledged && !self.health.borrow().plan_expired {
            if self
                .client
                .acknowledge_plan(&identity.credentials, revision)
                .await
                .is_ok()
            {
                self.acknowledged = revision;
            } else {
                failed = true;
            }
        }
        if Instant::now() >= self.next_work {
            self.next_work =
                Instant::now() + Duration::from_millis(self.config.agent.sync_interval_ms);
            match self.client.work(&identity.credentials).await {
                Ok(commands) => {
                    for command in commands {
                        if self.updates.try_send(Update::Work(command)).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => failed = true,
            }
        }
        let batch = self.store.batch().await?;
        if !batch.is_empty() {
            match self.client.upload(&identity.credentials, batch).await {
                Ok(ids) => self.store.acknowledge(ids).await?,
                Err(_) => failed = true,
            }
        }
        if Instant::now() >= self.next_health {
            self.next_health = Instant::now() + Duration::from_secs(30);
            let health = self.health.borrow().clone();
            if self
                .client
                .health(&identity.credentials, &health)
                .await
                .is_err()
            {
                failed = true;
            }
        }
        anyhow::ensure!(!failed, "one or more control operations failed");
        Ok(())
    }

    async fn fetch(&self, identity: &Identity) -> Result<()> {
        let revision = *self.revision.borrow();
        if let Some(plan) = self.client.plan(&identity.credentials, revision).await? {
            let (applied, reply) = oneshot::channel();
            self.updates
                .send(Update::Plan { plan, applied })
                .await
                .context("executor stopped")?;
            reply
                .await
                .context("plan application interrupted")?
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }
}

async fn long_poll(
    client: Control,
    identity: watch::Receiver<Identity>,
    notifications: mpsc::Sender<()>,
    stop: CancellationToken,
) {
    let mut cursor = String::new();
    let mut backoff = Backoff::default();
    loop {
        let credentials = identity.borrow().credentials.clone();
        let result = tokio::select! {
            biased;
            () = stop.cancelled() => break,
            result = client.events(&credentials, &cursor) => result,
        };
        let delay = match result {
            Ok(event) => {
                backoff.reset();
                if let Some(event) = event {
                    if event.plan_changed || event.work_available {
                        let _ = notifications.try_send(());
                    }
                    cursor = event.cursor;
                }
                Duration::from_millis(250)
            }
            Err(_) => backoff.delay(),
        };
        tokio::select! { () = stop.cancelled() => break, () = tokio::time::sleep(delay) => {} }
    }
}
