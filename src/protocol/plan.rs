//! Full-snapshot validation and compilation, without network or persistent side effects.

use crate::config::{Config, HttpPolicy, ProbeKind, target};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Work categories remain distinct in admission and native observations.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Workload {
    #[default]
    Routine,
    Community,
}

/// A reusable HTTP execution policy; timing is per assigned agent, not fleet-wide.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub id: String,
    pub kind: ProbeKind,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    #[serde(default)]
    pub http: HttpPolicy,
}

/// An immutable assignment ID is bound to one monitor and target.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub id: String,
    pub monitor_id: String,
    pub recipe_id: String,
    pub target: String,
    pub phase_offset_ms: u64,
    #[serde(default)]
    pub jitter_ms: u64,
    #[serde(default)]
    pub workload: Workload,
}

/// Version 1 agent-specific desired state. A 304 never extends `valid_until_unix_ms`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema_version: u32,
    pub revision: u64,
    pub schedule_epoch_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    pub recipes: Vec<Recipe>,
    pub assignments: Vec<Assignment>,
}

/// A bounded, validated job snapshot retained by a running probe across plan changes.
#[derive(Clone, Debug)]
pub struct Check {
    pub assignment: Assignment,
    pub recipe: Recipe,
    pub revision: u64,
}

impl Plan {
    pub(crate) fn assigned(&self, id: &str) -> Result<Check> {
        let assignment = self
            .assignments
            .iter()
            .find(|assignment| assignment.id == id)
            .context("command is not assigned to this agent")?;
        let recipe = self
            .recipes
            .iter()
            .find(|recipe| recipe.id == assignment.recipe_id)
            .context("accepted assignment recipe missing")?;
        Ok(Check {
            assignment: assignment.clone(),
            recipe: recipe.clone(),
            revision: self.revision,
        })
    }

    /// Validate the complete snapshot before making any of it executable.
    /// # Errors
    /// Rejects schema, lease, identity, target, schedule, or resource violations.
    pub fn compile(&self, config: &Config, now: u64) -> Result<Vec<Check>> {
        ensure!(
            self.schema_version == 1 && self.revision > 0,
            "unsupported plan schema or revision"
        );
        ensure!(
            self.valid_until_unix_ms > now
                && self.valid_until_unix_ms - now <= config.limits.max_plan_lease_ms,
            "plan lease exceeds the local authorization window"
        );
        ensure!(
            self.schedule_epoch_unix_ms <= self.valid_until_unix_ms,
            "schedule epoch exceeds lease"
        );
        ensure!(
            self.assignments.len() <= config.limits.max_checks && self.recipes.len() <= 256,
            "plan exceeds local assignment or recipe limit"
        );
        let mut recipes = HashMap::new();
        for recipe in &self.recipes {
            super::identifier(&recipe.id)?;
            ensure!(
                recipes.insert(&recipe.id, recipe).is_none(),
                "duplicate recipe id"
            );
            ensure!(
                recipe.kind == ProbeKind::Http,
                "only HTTP recipes are implemented"
            );
            recipe.http.validate()?;
            ensure!(
                (config.limits.min_interval_ms..=86_400_000).contains(&recipe.interval_ms),
                "recipe interval violates local limits"
            );
            ensure!(
                recipe.timeout_ms > 0
                    && recipe.timeout_ms <= config.limits.max_timeout_ms
                    && recipe.timeout_ms <= recipe.interval_ms,
                "recipe timeout violates local limits"
            );
        }
        let mut ids = HashSet::new();
        self.assignments
            .iter()
            .map(|assignment| {
                super::identifier(&assignment.id)?;
                super::identifier(&assignment.monitor_id)?;
                ensure!(ids.insert(&assignment.id), "duplicate assignment id");
                let recipe = recipes
                    .get(&assignment.recipe_id)
                    .context("assignment references missing recipe")?;
                target::probe(ProbeKind::Http, &assignment.target)?;
                ensure!(
                    assignment.phase_offset_ms < recipe.interval_ms
                        && assignment.jitter_ms <= recipe.interval_ms / 20
                        && assignment.phase_offset_ms + assignment.jitter_ms < recipe.interval_ms,
                    "assignment phase offset or jitter is invalid"
                );
                Ok(Check {
                    assignment: assignment.clone(),
                    recipe: (*recipe).clone(),
                    revision: self.revision,
                })
            })
            .collect()
    }

    /// Preserve stable assignment identity when applying a newer full snapshot.
    /// # Errors
    /// Rejects rollback, reused revisions with different content, or identity substitution.
    pub fn follows(&self, previous: &Self) -> Result<()> {
        ensure!(self.revision >= previous.revision, "plan revision rollback");
        if self.revision == previous.revision {
            ensure!(
                self == previous,
                "a plan revision cannot change its content or lease"
            );
        }
        let old: HashMap<_, _> = previous
            .assignments
            .iter()
            .map(|assignment| (&assignment.id, assignment))
            .collect();
        for assignment in &self.assignments {
            if let Some(existing) = old.get(&assignment.id) {
                ensure!(
                    existing.monitor_id == assignment.monitor_id
                        && existing.target == assignment.target,
                    "assignment id cannot be reassigned to a different monitor or target"
                );
            }
        }
        Ok(())
    }
}
