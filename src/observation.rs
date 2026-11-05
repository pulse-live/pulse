//! Native monitoring observations, independent of diagnostic OpenTelemetry.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// UTC milliseconds, saturating only if the host clock is outside supported bounds.
#[must_use]
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Closed native outcome categories; unspecified wire values are never successes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, prost::Enumeration)]
#[repr(i32)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Outcome {
    Unspecified = 0,
    Pass = 1,
    Fail = 2,
    Error = 3,
    Skipped = 4,
}

/// Work provenance prevents interactive results from becoming routine uptime samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, prost::Enumeration)]
#[repr(i32)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Unspecified = 0,
    Routine = 1,
    Live = 2,
    Community = 3,
    Manual = 4,
}

/// A real probe outcome or an explicit local refusal, never an invented measurement.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, prost::Message)]
pub struct Observation {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub assignment_id: String,
    #[prost(string, tag = "3")]
    pub monitor_id: String,
    #[prost(uint64, tag = "4")]
    pub revision: u64,
    #[prost(enumeration = "Origin", tag = "5")]
    #[serde(with = "origin_json")]
    pub origin: i32,
    #[prost(string, optional, tag = "6")]
    pub command_id: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub run_id: Option<String>,
    #[prost(uint64, tag = "8")]
    pub started_unix_ms: u64,
    #[prost(uint64, tag = "9")]
    pub completed_unix_ms: u64,
    #[prost(uint64, tag = "10")]
    pub total_duration_us: u64,
    #[prost(enumeration = "Outcome", tag = "11")]
    #[serde(with = "outcome_json")]
    pub outcome: i32,
    #[prost(string, optional, tag = "12")]
    pub error: Option<String>,
    #[prost(uint32, optional, tag = "13")]
    pub http_status: Option<u32>,
    #[prost(string, optional, tag = "14")]
    pub resolved_address: Option<String>,
    #[prost(uint32, tag = "15")]
    pub redirects: u32,
    #[prost(uint64, tag = "16")]
    pub inspected_bytes: u64,
    #[prost(uint64, optional, tag = "17")]
    pub content_length: Option<u64>,
    #[prost(bool, tag = "18")]
    pub body_truncated: bool,
    #[prost(uint64, optional, tag = "19")]
    pub dns_duration_us: Option<u64>,
    #[prost(uint64, optional, tag = "20")]
    pub tcp_duration_us: Option<u64>,
    #[prost(uint64, optional, tag = "21")]
    pub tls_duration_us: Option<u64>,
    #[prost(uint64, optional, tag = "22")]
    pub ttfb_us: Option<u64>,
}

impl Observation {
    /// Create correlation metadata before a real execution or explicit local refusal.
    #[must_use]
    pub fn new(check: &crate::protocol::Check, command: Option<&crate::protocol::Command>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            assignment_id: check.assignment.id.clone(),
            monitor_id: check.assignment.monitor_id.clone(),
            revision: check.revision,
            origin: if command.is_some() {
                Origin::Live
            } else if check.assignment.workload == crate::protocol::Workload::Community {
                Origin::Community
            } else {
                Origin::Routine
            }
            .into(),
            command_id: command.map(|command| command.id.clone()),
            run_id: command.map(|command| command.run_id.clone()),
            started_unix_ms: unix_ms(),
            completed_unix_ms: 0,
            total_duration_us: 0,
            outcome: Outcome::Error.into(),
            error: None,
            http_status: None,
            resolved_address: None,
            redirects: 0,
            inspected_bytes: 0,
            content_length: None,
            body_truncated: false,
            dns_duration_us: None,
            tcp_duration_us: None,
            tls_duration_us: None,
            ttfb_us: None,
        }
    }
}

/// Wire batch; acknowledgement refers to individual stable observation IDs.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Batch {
    #[prost(uint32, tag = "1")]
    pub schema_version: u32,
    #[prost(message, repeated, tag = "2")]
    pub observations: Vec<Observation>,
}

/// Bounded counters exposed as machine-readable health, including lost work.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Health {
    pub phase: Phase,
    pub revision: u64,
    pub active_probes: usize,
    pub peak_active_probes: usize,
    pub completed: u64,
    pub skipped: u64,
    pub missed: u64,
    pub dropped: u64,
    pub plan_expired: bool,
}

/// Execution state; offline agents may continue work only within their accepted lease.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    WaitingForPlan,
    Active,
    Offline,
    LeaseExpired,
    Stopping,
    Stopped,
}

mod outcome_json {
    use super::Outcome;
    use serde::{Deserialize, Serialize};
    pub fn serialize<S: serde::Serializer>(
        value: impl std::borrow::Borrow<i32>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        Outcome::try_from(*value.borrow())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<i32, D::Error> {
        Outcome::deserialize(deserializer).map(i32::from)
    }
}
mod origin_json {
    use super::Origin;
    use serde::{Deserialize, Serialize};
    pub fn serialize<S: serde::Serializer>(
        value: impl std::borrow::Borrow<i32>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        Origin::try_from(*value.borrow())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<i32, D::Error> {
        Origin::deserialize(deserializer).map(i32::from)
    }
}
