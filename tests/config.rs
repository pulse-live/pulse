use anyhow::Result;
use pulse::config::{Config, LogLevel, Mode, ProbeKind};

const EXAMPLE: &str = include_str!("../examples/pulse.toml");

#[test]
fn example_and_minimal_configuration_are_valid() -> Result<()> {
    let example = Config::parse(EXAMPLE)?;
    assert_eq!(example.agent.mode, Mode::Standalone);
    assert_eq!(example.probes.len(), 2);
    assert_eq!(example.logging.level, LogLevel::Error);
    let minimal = Config::parse("[agent]\nmode = 'standalone'")?;
    assert_eq!(minimal.probes, []);
    assert_eq!(minimal.limits.max_concurrent_probes, 4);
    assert_eq!(minimal.limits.max_checks, 1024);
    assert_eq!(minimal.logging.level, LogLevel::Error);
    Ok(())
}

#[test]
fn managed_configuration_accepts_alternative_control_planes() -> Result<()> {
    let config =
        Config::parse("[agent]\nmode = 'managed'\ncontrol_plane_url = 'https://control.example'")?;
    assert_eq!(config.agent.mode, Mode::Managed);
    assert_eq!(config.probes, []);
    Ok(())
}

#[test]
fn modes_reject_incompatible_options() {
    for source in [
        "[agent]\nmode = 'managed'",
        "[agent]\nmode = 'standalone'\ncontrol_plane_url = 'https://control.example'",
        "[agent]\nmode = 'managed'\ncontrol_plane_url = 'http://control.example'",
        "[agent]\nmode = 'managed'\ncontrol_plane_url = 'https://control.example?token=secret'",
    ] {
        assert!(Config::parse(source).is_err());
    }
    let source = EXAMPLE.replace(
        "mode = \"standalone\"",
        "mode = \"managed\"\ncontrol_plane_url = \"https://control.example\"",
    );
    assert!(Config::parse(&source).is_err());
}

#[test]
fn invalid_limits_and_timing_report_the_violated_field() {
    for (old, replacement, field) in [
        (
            "max_concurrent_probes = 4",
            "max_concurrent_probes = 0",
            "max_concurrent_probes",
        ),
        (
            "max_concurrent_probes = 4",
            "max_concurrent_probes = 257",
            "max_concurrent_probes",
        ),
        ("max_checks = 128", "max_checks = 0", "max_checks"),
        ("max_checks = 128", "max_checks = 4097", "max_checks"),
        (
            "max_checks = 128",
            "max_checks = 1",
            "max_concurrent_probes",
        ),
        (
            "min_interval_ms = 1000",
            "min_interval_ms = 999",
            "min_interval_ms",
        ),
        (
            "min_interval_ms = 1000",
            "min_interval_ms = 86400001",
            "min_interval_ms",
        ),
        (
            "max_timeout_ms = 10000",
            "max_timeout_ms = 0",
            "max_timeout_ms",
        ),
        (
            "max_timeout_ms = 10000",
            "max_timeout_ms = 300001",
            "max_timeout_ms",
        ),
        (
            "max_result_bytes = 65536",
            "max_result_bytes = 1023",
            "max_result_bytes",
        ),
        (
            "max_result_bytes = 65536",
            "max_result_bytes = 1048577",
            "max_result_bytes",
        ),
        ("interval_ms = 300000", "interval_ms = 999", "interval_ms"),
        (
            "interval_ms = 300000",
            "interval_ms = 86400001",
            "interval_ms",
        ),
        ("timeout_ms = 5000", "timeout_ms = 0", "timeout_ms"),
        ("timeout_ms = 5000", "timeout_ms = 10001", "timeout_ms"),
        ("interval_ms = 300000", "interval_ms = 1000", "timeout_ms"),
    ] {
        let error = Config::parse(&EXAMPLE.replace(old, replacement))
            .err()
            .map(|error| error.to_string());
        assert!(
            error.as_deref().is_some_and(|error| error.contains(field)),
            "{replacement}: {error:?}"
        );
    }
}

#[test]
fn probe_identity_and_count_are_bounded() -> Result<()> {
    let mut config = Config::parse(EXAMPLE)?;
    let source = format!(
        "{EXAMPLE}\n[[probes]]\nid = 'homepage'\nkind = 'dns'\ntarget = 'example.com'\ninterval_ms = 300000\ntimeout_ms = 1000\n"
    );
    assert!(Config::parse(&source).is_err());
    config.probes.extend(Config::parse(EXAMPLE)?.probes);
    config.probes.extend(Config::parse(EXAMPLE)?.probes);
    config.probes.extend(Config::parse(EXAMPLE)?.probes);
    config.probes.extend(Config::parse(EXAMPLE)?.probes);
    config.limits.max_checks = 4;
    let error = config.validate().err().map(|error| error.to_string());
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("probe count"))
    );
    for id in ["", "contains spaces", "line\nbreak", &"x".repeat(65)] {
        let mut config = Config::parse(EXAMPLE)?;
        if let Some(probe) = config.probes.first_mut() {
            probe.id = id.into();
        }
        assert!(config.validate().is_err());
    }
    Ok(())
}

#[test]
fn schema_is_strict_and_parse_errors_do_not_echo_values() {
    for source in [
        "",
        "[agent",
        "[agent]\nmode = 'unknown'",
        "[agent]\nmode = 'standalone'\ntoken = 'secret-never-log'",
        "[agent]\nmode = 'standalone'\n[logging]\nlevel = 'verbose'",
        "[agent]\nmode = 'standalone'\n[limits]\nmax_checks = -1",
        "[agent]\nmode = 'standalone'\n[limits]\nmax_cheks = 10",
        "[agent]\nmode = 'standalone'\n[unexpected]\nvalue = 1",
    ] {
        let error = Config::parse(source)
            .err()
            .map(|error| format!("{error:#}"));
        assert!(error.is_some(), "{source}");
        assert!(
            !error
                .as_deref()
                .is_some_and(|error| error.contains("secret-never-log"))
        );
    }
    assert!(Config::parse(&" ".repeat(1_048_577)).is_err());
}

#[test]
fn all_planned_protocol_targets_validate_without_network_access() -> Result<()> {
    for (kind, target) in [
        (ProbeKind::Http, "http://example.com/path?key=value"),
        (ProbeKind::Http, "https://[::1]:8443"),
        (ProbeKind::Tcp, "127.0.0.1:443"),
        (ProbeKind::Tcp, "[2001:db8::1]:443"),
        (ProbeKind::Dns, "example.com."),
        (ProbeKind::Tls, "example.com:443"),
    ] {
        let mut config = Config::parse(EXAMPLE)?;
        if let Some(probe) = config.probes.first_mut() {
            probe.kind = kind;
            probe.target = target.into();
        }
        config.validate()?;
    }
    Ok(())
}

#[test]
fn invalid_protocol_targets_are_rejected() -> Result<()> {
    for (kind, target) in [
        (ProbeKind::Http, "file:///etc/passwd"),
        (ProbeKind::Http, "https://user:secret@example.com"),
        (ProbeKind::Http, "https://example.com/#fragment"),
        (ProbeKind::Http, "https://"),
        (ProbeKind::Http, "https://example.com\n"),
        (ProbeKind::Tcp, "example.com:0"),
        (ProbeKind::Tcp, "example.com:65536"),
        (ProbeKind::Tls, "[invalid]:443"),
        (ProbeKind::Dns, "-example.com"),
        (ProbeKind::Dns, "example..com"),
        (ProbeKind::Dns, "café.example"),
    ] {
        let mut config = Config::parse(EXAMPLE)?;
        if let Some(probe) = config.probes.first_mut() {
            probe.kind = kind;
            probe.target = target.into();
        }
        assert!(config.validate().is_err(), "{kind:?}: {target}");
    }
    Ok(())
}
