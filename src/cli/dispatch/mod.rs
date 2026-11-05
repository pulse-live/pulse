//! Translate parsed arguments into validated actions without I/O.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::ArgMatches;

use crate::{
    cli::actions::{Action, CommunityOperation},
    config::{ProbeKind, target},
};

/// Construct an action from Pulse command matches.
///
/// # Errors
/// Returns an error for missing arguments, unsupported commands, or invalid targets.
pub fn handler(matches: &ArgMatches) -> Result<Action> {
    match matches.subcommand() {
        Some(("run", args)) => Ok(Action::Run {
            config: path(args, "config")?,
        }),
        Some(("enroll", args)) => {
            let server = string(args, "server")?;
            target::control_plane(&server)?;
            Ok(Action::Enroll {
                server,
                token: crate::protocol::Secret(string(args, "token")?),
                state_dir: path(args, "state-dir")?,
                ca_file: args.try_get_one::<PathBuf>("ca-file")?.cloned(),
            })
        }
        Some(("probe", args)) => {
            let (name, args) = args.subcommand().context("a probe protocol is required")?;
            let protocol = match name {
                "http" => ProbeKind::Http,
                "tcp" => ProbeKind::Tcp,
                "dns" => ProbeKind::Dns,
                "tls" => ProbeKind::Tls,
                _ => bail!("unsupported probe protocol"),
            };
            let target = string(args, "target")?;
            target::probe(protocol, &target)?;
            Ok(Action::Probe {
                protocol,
                target,
                allow_private: args.get_flag("allow-private"),
            })
        }
        Some(("config", args)) => match args.subcommand() {
            Some(("validate", args)) => Ok(Action::ConfigValidate {
                file: path(args, "file")?,
            }),
            _ => bail!("a configuration subcommand is required"),
        },
        Some(("status", args)) => Ok(Action::Status {
            state_dir: path(args, "state-dir")?,
        }),
        Some(("community", args)) => {
            let (operation, args) = args.subcommand().context("community operation required")?;
            Ok(Action::Community {
                operation: match operation {
                    "enable" => CommunityOperation::Enable,
                    "status" => CommunityOperation::Status,
                    "disable" => CommunityOperation::Disable,
                    _ => bail!("unknown community operation"),
                },
                state_dir: path(args, "state-dir")?,
            })
        }
        _ => bail!("a command is required; see --help"),
    }
}

fn string(matches: &ArgMatches, name: &str) -> Result<String> {
    matches
        .try_get_one::<String>(name)?
        .cloned()
        .context("missing required argument")
}

fn path(matches: &ArgMatches, name: &str) -> Result<PathBuf> {
    matches
        .try_get_one::<PathBuf>(name)?
        .cloned()
        .context("missing configuration path")
}
