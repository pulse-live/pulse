use std::{env, ffi::OsString};

use anyhow::Result;
use clap::ColorChoice;

use crate::cli::{
    actions::Action,
    commands, dispatch,
    telemetry::{self, Telemetry},
};

/// Parse arguments, initialize diagnostics, and return an action for execution.
///
/// The caller owns the telemetry lifetime and must await its shutdown even on failure.
/// Clap handles help, version, and syntax errors before telemetry initialization.
///
/// # Errors
/// Returns telemetry initialization errors or invalid typed action arguments.
pub fn start(telemetry: &mut Telemetry) -> Result<Action> {
    let args: Vec<OsString> = env::args_os().collect();
    let color = color_choice(&args, env::var_os("NO_COLOR").is_some());
    let matches = commands::new().color(color).get_matches_from(args);
    let verbosity = telemetry::verbosity_level(matches.get_count("verbose"));
    *telemetry = telemetry::init(verbosity, color)?;
    dispatch::handler(&matches)
}

// Resolve color before clap's early help/error exits. Stop at '--' so literal values
// cannot change styling. Conflicting color flags are rejected by clap itself.
fn color_choice(args: &[OsString], no_color: bool) -> ColorChoice {
    let default = if no_color {
        ColorChoice::Never
    } else {
        ColorChoice::Auto
    };
    args.iter()
        .take_while(|arg| *arg != "--")
        .fold(default, |choice, arg| {
            if arg == "--no-color" {
                ColorChoice::Never
            } else if arg == "--color" {
                ColorChoice::Always
            } else {
                choice
            }
        })
}

#[cfg(test)]
mod tests {
    use super::color_choice;
    use clap::ColorChoice;

    #[test]
    fn color_flags_override_environment_and_respect_literal_values() {
        assert_eq!(color_choice(&[], true), ColorChoice::Never);
        assert_eq!(color_choice(&[], false), ColorChoice::Auto);
        assert_eq!(color_choice(&["--color".into()], true), ColorChoice::Always);
        assert_eq!(
            color_choice(&["--no-color".into()], false),
            ColorChoice::Never
        );
        assert_eq!(
            color_choice(&["--".into(), "--color".into()], true),
            ColorChoice::Never
        );
    }
}
