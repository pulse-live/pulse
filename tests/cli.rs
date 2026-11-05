use std::{
    env,
    path::PathBuf,
    process::{Command, Output},
};

use anyhow::{Context as _, Result};
use clap::error::ErrorKind;
use pulse::{
    cli::{actions::Action, commands, dispatch},
    config::ProbeKind,
};

fn pulse() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pulse"));
    // Isolate child processes without mutating the test runner's environment.
    for (name, _) in env::vars_os() {
        if name.to_string_lossy().starts_with("OTEL_") {
            command.env_remove(name);
        }
    }
    for name in [
        "RUST_LOG",
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
        "FORCE_COLOR",
    ] {
        command.env_remove(name);
    }
    command
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn help_and_version_use_clap_metadata() -> Result<()> {
    commands::new().debug_assert();
    for flag in ["--help", "-V", "--version"] {
        let output = pulse().arg(flag).output()?;
        assert!(output.status.success(), "{}", output_text(&output));
        let stdout = String::from_utf8(output.stdout)?;
        if flag == "--help" {
            for text in [
                "probe",
                "run",
                "enroll",
                "config",
                "--verbose",
                "--color",
                "--no-color",
                "status",
            ] {
                assert!(stdout.contains(text), "missing help text: {text}");
            }
        } else if flag == "-V" {
            assert_eq!(
                stdout.trim(),
                concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"))
            );
        } else {
            let hash = stdout
                .trim()
                .strip_prefix(concat!(
                    env!("CARGO_PKG_NAME"),
                    " ",
                    env!("CARGO_PKG_VERSION"),
                    " - "
                ))
                .context("long version must include the package version and Git commit")?;
            assert!(
                hash == "unknown"
                    || (hash.len() == 40 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())),
                "invalid build commit: {hash}"
            );
        }
    }
    Ok(())
}

#[test]
fn nested_help_works_without_initializing_telemetry() -> Result<()> {
    for args in [
        vec!["probe", "--help"],
        vec!["probe", "http", "--help"],
        vec!["config", "validate", "--help"],
    ] {
        let output = pulse()
            .args(args)
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", "invalid")
            .output()?;
        assert!(output.status.success(), "{}", output_text(&output));
    }
    Ok(())
}

#[test]
fn every_command_dispatches_to_a_typed_action() -> Result<()> {
    for (args, expected) in [
        (
            vec!["pulse", "run", "--config", "pulse.toml"],
            Action::Run {
                config: PathBuf::from("pulse.toml"),
            },
        ),
        (
            vec!["pulse", "config", "validate", "--file", "pulse.toml"],
            Action::ConfigValidate {
                file: PathBuf::from("pulse.toml"),
            },
        ),
        (
            vec![
                "pulse",
                "enroll",
                "--server",
                "https://control.example",
                "--token",
                "test-token",
            ],
            Action::Enroll {
                server: "https://control.example".into(),
                token: pulse::protocol::Secret("test-token".into()),
                state_dir: PathBuf::from(".pulse"),
                ca_file: None,
            },
        ),
    ] {
        assert_eq!(
            dispatch::handler(&commands::new().try_get_matches_from(args)?)?,
            expected
        );
    }
    {
        let (name, protocol, target) = ("http", ProbeKind::Http, "https://example.com");
        let args = commands::new().try_get_matches_from(["pulse", "probe", name, target])?;
        assert_eq!(
            dispatch::handler(&args)?,
            Action::Probe {
                protocol,
                target: target.into(),
                allow_private: false
            }
        );
    }
    Ok(())
}

#[test]
fn missing_arguments_and_unknown_commands_fail() {
    for args in [
        vec!["pulse"],
        vec!["pulse", "probe"],
        vec!["pulse", "probe", "http"],
        vec!["pulse", "run"],
        vec!["pulse", "enroll"],
        vec!["pulse", "config"],
        vec!["pulse", "probe", "icmp", "example.com"],
        vec!["pulse", "unknown"],
        vec!["pulse", "--color", "--no-color", "run", "--config", "file"],
    ] {
        assert!(
            commands::new().try_get_matches_from(args.clone()).is_err(),
            "{args:?}"
        );
    }
}

#[test]
fn dispatch_rejects_invalid_targets_without_io() -> Result<()> {
    for args in [
        vec!["pulse", "probe", "http", "ftp://example.com"],
        vec!["pulse", "probe", "http", "https://user:secret@example.com"],
        vec!["pulse", "probe", "http", "https://example.com:0"],
        vec![
            "pulse",
            "enroll",
            "--server",
            "http://example.com",
            "--token",
            "test-token",
        ],
        vec![
            "pulse",
            "enroll",
            "--server",
            "https://example.com?token=secret",
            "--token",
            "test-token",
        ],
    ] {
        let matches = commands::new().try_get_matches_from(args.clone())?;
        assert!(dispatch::handler(&matches).is_err(), "{args:?}");
    }
    let matches = clap::Command::new("pulse").try_get_matches_from(["pulse"])?;
    assert!(dispatch::handler(&matches).is_err());
    Ok(())
}

#[test]
fn global_options_work_at_nested_positions() -> Result<()> {
    for args in [
        vec!["pulse", "-vvv", "probe", "http", "https://example.com"],
        vec!["pulse", "probe", "-vvv", "http", "https://example.com"],
        vec![
            "pulse",
            "probe",
            "http",
            "https://example.com",
            "-vvv",
            "--no-color",
        ],
    ] {
        let matches = commands::new().try_get_matches_from(args)?;
        assert_eq!(matches.get_count("verbose"), 3);
        assert!(matches!(
            dispatch::handler(&matches)?,
            Action::Probe {
                protocol: ProbeKind::Http,
                ..
            }
        ));
    }
    Ok(())
}

#[test]
fn clap_reports_help_and_version_as_display_events() {
    for (flag, kind) in [
        ("--help", ErrorKind::DisplayHelp),
        ("--version", ErrorKind::DisplayVersion),
    ] {
        assert_eq!(
            commands::new()
                .try_get_matches_from(["pulse", flag])
                .err()
                .map(|error| error.kind()),
            Some(kind)
        );
    }
}

#[test]
fn unsupported_protocols_fail_without_execution() -> Result<()> {
    for protocol in ["tcp", "dns", "tls", "icmp"] {
        let output = pulse().args(["probe", protocol, "example.com"]).output()?;
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stdout, b"");
    }
    Ok(())
}

#[test]
fn validation_is_functional_and_local_logging_obeys_filters() -> Result<()> {
    let args = ["config", "validate", "--file", "examples/pulse.toml"];
    let output = pulse().args(args).output()?;
    assert!(output.status.success(), "{}", output_text(&output));
    assert_eq!(
        String::from_utf8(output.stdout)?.trim(),
        "Configuration is valid"
    );
    assert_eq!(output.stderr, b"");

    let verbose = pulse().args(args).arg("-v").output()?;
    assert!(verbose.status.success());
    assert!(String::from_utf8_lossy(&verbose.stderr).contains("INFO"));
    let quiet = pulse()
        .args(args)
        .arg("-vvv")
        .env("RUST_LOG", "error")
        .output()?;
    assert!(quiet.status.success());
    assert_eq!(quiet.stderr, b"");
    let invalid = pulse().args(args).env("RUST_LOG", "not[a[valid").output()?;
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid RUST_LOG"));
    Ok(())
}

#[test]
fn color_applies_to_help_and_logs_with_environment_precedence() -> Result<()> {
    for (flag, colored) in [("--color", true), ("--no-color", false)] {
        let help = pulse()
            .args([flag, "--help"])
            .env("NO_COLOR", "1")
            .output()?;
        assert!(help.status.success());
        assert_eq!(
            String::from_utf8_lossy(&help.stdout).contains('\u{1b}'),
            colored
        );
        let logs = pulse()
            .args([
                "config",
                "validate",
                "--file",
                "examples/pulse.toml",
                "-v",
                flag,
            ])
            .env("NO_COLOR", "1")
            .output()?;
        assert!(logs.status.success());
        assert_eq!(
            String::from_utf8_lossy(&logs.stderr).contains('\u{1b}'),
            colored
        );
    }
    let output = pulse()
        .args(["--help"])
        .env("NO_COLOR", "1")
        .env("CLICOLOR_FORCE", "1")
        .output()?;
    assert!(!String::from_utf8_lossy(&output.stdout).contains('\u{1b}'));
    Ok(())
}

#[test]
fn exporter_requires_both_compile_time_capability_and_explicit_configuration() -> Result<()> {
    let output = pulse()
        .args(["config", "validate", "--file", "examples/pulse.toml"])
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "invalid-endpoint")
        .output()?;
    assert_eq!(output.status.success(), !cfg!(feature = "telemetry"));
    #[cfg(feature = "telemetry")]
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid OTEL_EXPORTER_OTLP_ENDPOINT")
    );
    Ok(())
}

#[test]
fn signal_specific_endpoint_alone_does_not_opt_in() -> Result<()> {
    let output = pulse()
        .args(["config", "validate", "--file", "examples/pulse.toml"])
        .env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "invalid-endpoint")
        .output()?;
    assert!(output.status.success(), "{}", output_text(&output));
    Ok(())
}

#[cfg(feature = "telemetry")]
#[test]
fn grpc_exporter_initializes_with_no_collector_or_spans() -> Result<()> {
    // connect_lazy and a disabled span filter permit a real initialization/shutdown
    // check with no network traffic and no listening collector.
    for endpoint in ["https://127.0.0.1:4317", "http://127.0.0.1:4317"] {
        let output = pulse()
            .args(["config", "validate", "--file", "examples/pulse.toml"])
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)
            .env("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")
            .env("RUST_LOG", "off")
            .output()?;
        assert!(output.status.success(), "{}", output_text(&output));
    }
    let invalid = pulse()
        .args(["config", "validate", "--file", "examples/pulse.toml"])
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4317")
        .env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf")
        .output()?;
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("must be grpc"));
    Ok(())
}

#[test]
fn configuration_file_failures_return_nonzero_without_exposing_input() -> Result<()> {
    use std::{fs, io::Write as _};

    // A unique temporary directory contains all fixtures and is cleaned on every exit.
    struct FixtureDirectory(PathBuf);
    impl Drop for FixtureDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let directory_path = env::temp_dir().join(format!("pulse-config-tests-{}", std::process::id()));
    fs::create_dir(&directory_path)?;
    let directory = FixtureDirectory(directory_path);
    let path = directory.0.join("pulse.toml");
    for (contents, message) in [
        (
            b"[agent]\nmode = 'standalone'\nunknown = 'secret-never-log'".as_slice(),
            "invalid TOML",
        ),
        (b"\xff".as_slice(), "UTF-8"),
    ] {
        fs::write(&path, contents)?;
        let output = pulse()
            .args(["config", "validate", "--file"])
            .arg(&path)
            .output()?;
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "{stderr}");
        assert!(!stderr.contains("secret-never-log"));
        assert_eq!(output.stdout, b"");
    }
    let mut file = fs::File::create(&path)?;
    file.write_all(&vec![b' '; 1_048_577])?;
    drop(file);
    let oversized = pulse()
        .args(["config", "validate", "--file"])
        .arg(&path)
        .output()?;
    assert_eq!(oversized.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("exceeds 1 MiB"));
    let missing = pulse()
        .args(["config", "validate", "--file"])
        .arg(directory.0.join("missing.toml"))
        .output()?;
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("cannot open configuration"));
    Ok(())
}
