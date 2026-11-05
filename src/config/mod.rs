//! Strict local configuration parsing. Validation never performs network I/O.

mod runtime;
pub(crate) mod target;
pub use runtime::{Community, HttpPolicy, Method, Network, Output, StatusRange};

use std::{collections::HashSet, path::PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

/// Maximum configuration input size, enforced before TOML parsing.
pub const MAX_CONFIG_BYTES: u64 = 1_048_576;
const MAX_INTERVAL_MS: u64 = 86_400_000;

/// Local configuration and security policy; remote plans cannot raise these ceilings.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub agent: Agent,
    #[serde(default)]
    #[serde(alias = "resources")]
    pub limits: Limits,
    #[serde(default)]
    pub probes: Vec<Probe>,
    #[serde(default)]
    pub logging: Logging,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub output: Output,
    #[serde(default)]
    pub community: Community,
}

/// Operating mode and explicit control-plane destination.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub mode: Mode,
    pub control_plane_url: Option<String>,
    #[serde(default = "runtime::state_dir")]
    pub state_dir: PathBuf,
    pub ca_file: Option<PathBuf>,
    #[serde(default = "runtime::sync_interval")]
    pub sync_interval_ms: u64,
    #[serde(default = "runtime::yes")]
    pub long_poll: bool,
}

/// The same binary will support local and centrally assigned workloads.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Standalone,
    Managed,
}

/// Local ceilings shared by recurring and on-demand work.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_concurrent_probes: usize,
    pub max_checks: usize,
    pub min_interval_ms: u64,
    pub max_timeout_ms: u64,
    pub max_result_bytes: u64,
    pub max_probe_starts_per_second: u32,
    pub max_connections: usize,
    pub max_response_bytes: usize,
    pub max_spool_mb: u32,
    pub max_buffered_observations: usize,
    pub reserved_urgent_probes: usize,
    pub max_bytes_per_second: usize,
    pub max_plan_lease_ms: u64,
    pub max_probe_memory_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_concurrent_probes: 4,
            max_checks: 1024,
            min_interval_ms: 1_000,
            max_timeout_ms: 10_000,
            max_result_bytes: 65_536,
            max_probe_starts_per_second: 5,
            max_connections: 8,
            max_response_bytes: 32_768,
            max_spool_mb: 32,
            max_buffered_observations: 256,
            reserved_urgent_probes: 1,
            max_bytes_per_second: 1_048_576,
            max_plan_lease_ms: 86_400_000,
            max_probe_memory_bytes: 16_777_216,
        }
    }
}

/// A recurring local check; HTTP execution is implemented, other engines are deferred.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub id: String,
    pub kind: ProbeKind,
    pub target: String,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    #[serde(default)]
    pub phase_offset_ms: Option<u64>,
    #[serde(default)]
    pub jitter_ms: u64,
    #[serde(default)]
    pub http: HttpPolicy,
}

/// Protocol selectors shared by configuration and one-shot CLI actions.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeKind {
    Http,
    Tcp,
    Dns,
    Tls,
}

/// Planned daemon logging configuration; CLI diagnostics use flags and `RUST_LOG`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Logging {
    pub level: LogLevel,
}

/// Allowed diagnostic severity levels.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    #[default]
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Config {
    /// Parse TOML and validate the complete configuration.
    ///
    /// # Errors
    /// Rejects oversized input, malformed TOML, unknown fields, and invalid values.
    /// Errors omit input values to avoid disclosing embedded secrets.
    pub fn parse(contents: &str) -> Result<Self> {
        ensure!(
            u64::try_from(contents.len())? <= MAX_CONFIG_BYTES,
            "configuration exceeds 1 MiB"
        );
        let config: Self = toml::from_str(contents).map_err(|error: toml::de::Error| {
            let line = error.span().and_then(|span| contents.get(..span.start))
                .map_or(1, |prefix| prefix.bytes().filter(|byte| *byte == b'\n').count() + 1);
            anyhow!("invalid TOML or configuration schema at line {line}; check field names and types (values omitted)")
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Revalidate a configuration after local modification.
    ///
    /// # Errors
    /// Rejects incompatible modes, unsafe limits, duplicate checks, or invalid probe definitions.
    pub fn validate(&self) -> Result<()> {
        self.limits.validate()?;
        self.validate_runtime()?;
        match self.agent.mode {
            Mode::Standalone => ensure!(
                self.agent.control_plane_url.is_none(),
                "standalone mode cannot set agent.control_plane_url"
            ),
            Mode::Managed => {
                target::control_plane(
                    self.agent
                        .control_plane_url
                        .as_deref()
                        .context("managed mode requires agent.control_plane_url")?,
                )?;
                ensure!(
                    self.probes.is_empty(),
                    "managed mode cannot define local probes; assignments will come from the control plane"
                );
            }
        }
        ensure!(
            self.probes.len() <= self.limits.max_checks,
            "probe count exceeds limits.max_checks"
        );
        let mut ids = HashSet::new();
        for probe in &self.probes {
            ensure!(
                !probe.id.is_empty()
                    && probe.id.len() <= 64
                    && probe
                        .id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
                "probe id must contain 1..=64 ASCII letters, digits, hyphens, or underscores"
            );
            ensure!(ids.insert(&probe.id), "probe ids must be unique");
            ensure!(
                (self.limits.min_interval_ms..=MAX_INTERVAL_MS).contains(&probe.interval_ms),
                "probe interval_ms must be at least limits.min_interval_ms and at most 86400000"
            );
            ensure!(
                probe.timeout_ms > 0
                    && probe.timeout_ms <= self.limits.max_timeout_ms
                    && probe.timeout_ms <= probe.interval_ms,
                "probe timeout_ms must be positive and no greater than interval_ms or limits.max_timeout_ms"
            );
            target::probe(probe.kind, &probe.target)?;
            probe.http.validate()?;
            ensure!(
                probe.phase_offset_ms.unwrap_or(0) < probe.interval_ms
                    && probe.jitter_ms <= probe.interval_ms / 20
                    && probe.phase_offset_ms.unwrap_or(0) + probe.jitter_ms < probe.interval_ms,
                "invalid probe phase offset or jitter"
            );
        }
        Ok(())
    }
}

impl Limits {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            (1..=256).contains(&self.max_concurrent_probes),
            "limits.max_concurrent_probes must be in 1..=256"
        );
        ensure!(
            (1..=4096).contains(&self.max_checks),
            "limits.max_checks must be in 1..=4096"
        );
        ensure!(
            self.max_concurrent_probes <= self.max_checks,
            "limits.max_concurrent_probes cannot exceed limits.max_checks"
        );
        ensure!(
            (1_000..=MAX_INTERVAL_MS).contains(&self.min_interval_ms),
            "limits.min_interval_ms must be in 1000..=86400000"
        );
        ensure!(
            (1..=300_000).contains(&self.max_timeout_ms),
            "limits.max_timeout_ms must be in 1..=300000"
        );
        ensure!(
            (1_024..=1_048_576).contains(&self.max_result_bytes),
            "limits.max_result_bytes must be in 1024..=1048576"
        );
        Ok(())
    }
}
