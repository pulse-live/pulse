//! Local policy and HTTP options; remote plans cannot replace these ceilings.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;

use super::{Config, MAX_CONFIG_BYTES, Mode};

pub(super) fn state_dir() -> PathBuf {
    PathBuf::from(".pulse")
}
pub(super) const fn sync_interval() -> u64 {
    30_000
}
pub(super) const fn yes() -> bool {
    true
}

/// Explicit permission for private destinations; metadata addresses stay forbidden.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    pub allow_private_networks: bool,
    pub ca_file: Option<PathBuf>,
}

/// Local output is opt-in; records are JSON lines without response bodies.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Output {
    pub stdout: bool,
}

/// Community capacity is local policy, separate from enrollment identity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Community {
    pub enabled: bool,
    pub max_concurrent_probes: usize,
    pub max_checks_per_hour: u32,
}

impl Default for Community {
    fn default() -> Self {
        Self {
            enabled: false,
            max_concurrent_probes: 1,
            max_checks_per_hour: 60,
        }
    }
}

/// Supported request methods; arbitrary request bodies and headers are not accepted.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    #[default]
    Get,
    Head,
}

/// Inclusive expected HTTP status range.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct StatusRange {
    pub min: u16,
    pub max: u16,
}
impl Default for StatusRange {
    fn default() -> Self {
        Self { min: 200, max: 299 }
    }
}

/// Bounded HTTP policy. Redirects are followed manually and revalidated per hop.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct HttpPolicy {
    pub method: Method,
    pub expected_status: StatusRange,
    pub max_redirects: u8,
    pub inspect_body: bool,
}

impl HttpPolicy {
    /// Validate response policy without performing a request.
    /// # Errors
    /// Rejects invalid status ranges or excessive redirect limits.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (100..=599).contains(&self.expected_status.min)
                && self.expected_status.max >= self.expected_status.min
                && self.expected_status.max <= 599,
            "invalid expected HTTP status range"
        );
        ensure!(self.max_redirects <= 5, "max_redirects cannot exceed 5");
        Ok(())
    }
}

impl Config {
    /// Read at most 1 MiB and validate a local TOML file.
    /// # Errors
    /// Returns file, encoding, schema, and policy errors without exposing input values.
    pub async fn load(path: &Path) -> Result<Self> {
        let file = tokio::fs::File::open(path)
            .await
            .context("cannot open configuration file")?;
        let mut contents = String::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut contents)
            .await
            .context("cannot read configuration as UTF-8")?;
        Self::parse(&contents)
    }

    pub(super) fn validate_runtime(&self) -> Result<()> {
        let limits = &self.limits;
        ensure!(
            (4_194_304..=1_073_741_824).contains(&limits.max_probe_memory_bytes),
            "limits.max_probe_memory_bytes must be in 4194304..=1073741824"
        );
        ensure!(
            (1000..=604_800_000).contains(&limits.max_plan_lease_ms),
            "limits.max_plan_lease_ms must be in 1000..=604800000"
        );
        ensure!(
            (1..=1000).contains(&limits.max_probe_starts_per_second),
            "limits.max_probe_starts_per_second must be in 1..=1000"
        );
        ensure!(
            (2..=256).contains(&limits.max_connections),
            "limits.max_connections must be in 2..=256"
        );
        ensure!(
            (1..=1_048_576).contains(&limits.max_response_bytes),
            "limits.max_response_bytes must be in 1..=1048576"
        );
        ensure!(
            (1..=256).contains(&limits.max_spool_mb),
            "max_spool_mb must be in 1..=256"
        );
        ensure!(
            (1..=4096).contains(&limits.max_buffered_observations),
            "limits.max_buffered_observations must be in 1..=4096"
        );
        ensure!(
            limits.reserved_urgent_probes < limits.max_concurrent_probes,
            "limits.reserved_urgent_probes must be below max_concurrent_probes"
        );
        ensure!(
            (1024..=16_777_216).contains(&limits.max_bytes_per_second),
            "limits.max_bytes_per_second must be in 1024..=16777216"
        );
        ensure!(
            (1000..=300_000).contains(&self.agent.sync_interval_ms),
            "agent.sync_interval_ms must be in 1000..=300000"
        );
        ensure!(
            self.agent.mode != Mode::Managed || limits.max_connections >= 5,
            "managed mode needs three control connection slots and probe capacity"
        );
        ensure!(
            !self.community.enabled || self.agent.mode == Mode::Managed,
            "community participation requires managed enrollment"
        );
        ensure!(
            self.community.max_concurrent_probes > 0
                && self.community.max_concurrent_probes <= limits.max_concurrent_probes
                && (1..=3600).contains(&self.community.max_checks_per_hour),
            "community concurrency must fit global limits and max_checks_per_hour must be in 1..=3600"
        );
        ensure!(
            !self.agent.state_dir.as_os_str().is_empty(),
            "agent.state_dir cannot be empty"
        );
        Ok(())
    }
}
