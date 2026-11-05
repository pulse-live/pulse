//! Versioned HTTPS control contracts; identity comes from bearer authentication.
//! Plans are full snapshots, notifications are hints, and acknowledgements are explicit.

mod plan;
pub use plan::{Assignment, Check, Plan, Recipe, Workload};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A credential that deliberately redacts its debug representation.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(pub String);
impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

/// Provisioning role is returned by the authenticated enrollment service, never a CLI flag.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Deployment {
    Private,
    PublicEdge,
    Community,
}

/// Securely persisted credentials, bound to one exact HTTPS control-plane URL.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub schema_version: u32,
    pub agent_id: String,
    pub token: Secret,
    pub expires_unix_ms: u64,
    pub deployment: Deployment,
}

impl Credentials {
    /// Validate bounded identifiers and bearer syntax before persistence or use.
    /// # Errors
    /// Rejects incompatible, expired, empty, or malformed credentials.
    pub fn validate(&self, now: u64) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported credential schema");
        identifier(&self.agent_id)?;
        ensure!(
            self.expires_unix_ms > now,
            "agent credential expired; enroll again"
        );
        ensure!(
            !self.token.0.is_empty()
                && self.token.0.len() <= 4096
                && self.token.0.bytes().all(|byte| byte.is_ascii_graphic()),
            "invalid bearer credential"
        );
        Ok(())
    }
}

/// Bootstrap proof; contains no user-selected agent role or tenant identity.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    pub schema_version: u32,
    pub token: Secret,
    pub capabilities: Vec<String>,
}

/// Durable acknowledgement identifies exactly the records retained by the server.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Acknowledgement {
    pub schema_version: u32,
    pub accepted_ids: Vec<String>,
}

/// Long-poll response. Its cursor is bounded and never establishes authorization.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Events {
    pub cursor: String,
    pub plan_changed: bool,
    pub work_available: bool,
}

/// Urgency selects reserved capacity but cannot raise hard local limits.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Normal,
    #[default]
    Urgent,
}

/// A durable command may carry a cancellation tombstone for already claimed work.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub id: String,
    pub run_id: String,
    pub monitor_id: String,
    pub assignment_id: String,
    pub revision: u64,
    pub deadline_unix_ms: u64,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub cancelled: bool,
}

impl Command {
    /// Check correlation IDs and a short local execution lease before admission.
    /// # Errors
    /// Rejects expired work or deadlines more than one minute in the future.
    pub fn validate(&self, now: u64) -> Result<()> {
        for id in [
            &self.id,
            &self.run_id,
            &self.monitor_id,
            &self.assignment_id,
        ] {
            identifier(id)?;
        }
        ensure!(
            self.deadline_unix_ms > now && self.deadline_unix_ms - now <= 60_000,
            "invalid on-demand deadline"
        );
        Ok(())
    }
}

pub(crate) fn identifier(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "identifier must contain 1..=64 ASCII letters, digits, hyphens, or underscores"
    );
    Ok(())
}
