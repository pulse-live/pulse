//! Typed operations and their execution boundary.

mod config;
mod enroll;
mod probe;
mod run;
mod state;

use std::path::PathBuf;

use anyhow::Result;

use crate::{config::ProbeKind, protocol::Secret};

/// Supported operations with explicit execution and failure semantics.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Run autonomous HTTP checks from local or managed configuration.
    Run { config: PathBuf },
    /// Execute one HTTP probe under local network policy.
    Probe {
        protocol: ProbeKind,
        target: String,
        allow_private: bool,
    },
    /// Enroll or recover credentials with an explicitly selected control plane.
    Enroll {
        server: String,
        token: Secret,
        state_dir: PathBuf,
        ca_file: Option<PathBuf>,
    },
    /// Parse and validate a local TOML file.
    ConfigValidate { file: PathBuf },
    /// Inspect persisted local health without contacting the control plane.
    Status { state_dir: PathBuf },
    /// Set or inspect explicit local community participation.
    Community {
        operation: CommunityOperation,
        state_dir: PathBuf,
    },
}

/// Explicit local participation operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommunityOperation {
    Enable,
    Status,
    Disable,
}

impl Action {
    /// Execute the selected action.
    ///
    /// # Errors
    /// Returns validation, network, persistence, output, or diagnostic execution errors.
    #[tracing::instrument(name = "pulse.action", skip_all)]
    pub async fn execute(self) -> Result<()> {
        match self {
            Self::Run { config } => run::execute(&config).await,
            Self::Probe {
                protocol,
                target,
                allow_private,
            } => probe::execute(protocol, &target, allow_private).await,
            Self::Enroll {
                server,
                token,
                state_dir,
                ca_file,
            } => enroll::execute(&server, token, &state_dir, ca_file.as_deref()).await,
            Self::ConfigValidate { file } => config::execute(&file).await,
            Self::Status { state_dir } => state::status(&state_dir).await,
            Self::Community {
                operation,
                state_dir,
            } => state::community(operation, &state_dir).await,
        }
    }
}
