//! Admission precedes task creation; durable command IDs precede claims and execution.

use super::{Agent, Mode};
use crate::{
    governor::Class,
    observation::{Observation, Outcome, unix_ms},
    protocol::{Check, Command, Workload},
};
use anyhow::{Context, Result, ensure};
use tokio::time::{Duration, Instant};

impl Agent {
    pub(super) fn schedule_due(&mut self) -> Result<()> {
        while let Some((check, missed)) = self.scheduler.due(Instant::now())? {
            self.health.missed = self.health.missed.saturating_add(missed);
            let class = if check.assignment.workload == Workload::Community {
                Class::Community
            } else {
                Class::Routine
            };
            if self.health.plan_expired
                || self.active.contains(&check.assignment.id)
                || self.jobs.len() >= self.config.limits.max_concurrent_probes
            {
                self.health.skipped = self.health.skipped.saturating_add(1);
                continue;
            }
            match self
                .governor
                .admit(class, Instant::now(), self.community_enabled)
            {
                Ok(permit) => {
                    self.active.insert(check.assignment.id.clone());
                    let engine = if class == Class::Community {
                        self.public_engine.clone()
                    } else {
                        self.engine.clone()
                    };
                    let stop = if class == Class::Community {
                        self.community_stop.clone()
                    } else {
                        self.generation.clone()
                    };
                    let lease = self.lease;
                    self.jobs.spawn(async move {
                        let _permit = permit;
                        engine.execute(&check, None, &stop, lease).await
                    });
                    self.health.peak_active_probes =
                        self.health.peak_active_probes.max(self.jobs.len());
                }
                Err(_) => self.health.skipped = self.health.skipped.saturating_add(1),
            }
        }
        Ok(())
    }

    pub(super) async fn live(&mut self, command: Command) -> Result<()> {
        self.expire();
        ensure!(!self.health.plan_expired, "plan authorization expired");
        command.validate(unix_ms())?;
        if command.cancelled {
            if let Some((run, token)) = self.live_tokens.get(&command.id)
                && run == &command.run_id
            {
                token.cancel();
            }
            return Ok(());
        }
        if self.store.known_command(&command.id).await? {
            return Ok(());
        }
        let plan = self.plan.as_ref().context("no accepted plan")?;
        ensure!(
            command.revision == plan.revision,
            "command revision is not active"
        );
        let check: Check = plan.assigned(&command.assignment_id)?;
        ensure!(
            check.assignment.monitor_id == command.monitor_id,
            "command monitor mismatch"
        );
        ensure!(
            self.jobs.len() < self.config.limits.max_concurrent_probes,
            "probe task capacity exhausted"
        );
        let class = if check.assignment.workload == Workload::Community {
            Class::Community
        } else if command.priority == crate::protocol::Priority::Urgent {
            Class::Urgent
        } else {
            Class::Routine
        };
        let permit = self
            .governor
            .admit(class, Instant::now(), self.community_enabled)
            .map_err(|reason| anyhow::anyhow!("local admission rejected: {reason:?}"))?;
        let reservation = Observation::new(&check, Some(&command));
        if !self.store.reserve(&command, &reservation).await? {
            return Ok(());
        }
        let engine = if class == Class::Community {
            self.public_engine.clone()
        } else {
            self.engine.clone()
        };
        let parent = if class == Class::Community {
            self.community_stop.clone()
        } else {
            self.generation.clone()
        };
        let stop = parent.child_token();
        self.live_tokens
            .insert(command.id.clone(), (command.run_id.clone(), stop.clone()));
        let identity = self
            .identity
            .as_ref()
            .context("missing managed identity")?
            .borrow()
            .clone();
        let client = self.control.clone().context("no control client")?;
        let lease = self.lease;
        self.jobs.spawn(async move {
            let _permit = permit;
            let mut observation = reservation;
            let claim = tokio::select! {
                biased;
                () = stop.cancelled() => Err(anyhow::anyhow!("cancelled")),
                result = tokio::time::timeout(Duration::from_millis(command.deadline_unix_ms.saturating_sub(unix_ms())), client.claim(&identity.credentials, &command)) => {
                    result.map_err(|_| anyhow::anyhow!("claim deadline exceeded")).and_then(|result| result)
                }
            };
            if claim.is_ok() {
                let id = observation.id;
                observation = engine.execute(&check, Some(&command), &stop, lease).await;
                observation.id = id;
            } else {
                observation.set_outcome(Outcome::Skipped);
                observation.error = Some("claim_rejected_or_interrupted".into());
                observation.completed_unix_ms = unix_ms();
            }
            observation
        });
        self.health.peak_active_probes = self.health.peak_active_probes.max(self.jobs.len());
        Ok(())
    }

    pub(super) async fn record(&mut self, observation: Observation) -> Result<()> {
        if let Some(id) = &observation.command_id {
            self.live_tokens.remove(id);
        }
        if observation.outcome() == Outcome::Skipped {
            self.health.skipped = self.health.skipped.saturating_add(1);
        }
        if observation.command_id.is_none() {
            self.active.remove(&observation.assignment_id);
        }
        if self.config.agent.mode == Mode::Managed && !self.store.push(&observation).await? {
            self.health.dropped = self.health.dropped.saturating_add(1);
        }
        if let Some(output) = &self.output
            && output.try_send(observation).is_err()
        {
            self.health.dropped = self.health.dropped.saturating_add(1);
        }
        self.health.completed = self.health.completed.saturating_add(1);
        Ok(())
    }

    pub(super) async fn drain(&mut self) -> Result<()> {
        // Cancellation-aware probes finish promptly; any remaining tasks are bounded.
        while let Some(result) = self.jobs.join_next().await {
            self.record(result.context("probe shutdown task failed")?)
                .await?;
        }
        Ok(())
    }
}
