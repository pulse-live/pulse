[![Tests and build](https://github.com/pulse-live/pulse/actions/workflows/build.yml/badge.svg?branch=main)](https://github.com/pulse-live/pulse/actions/workflows/build.yml)
[![codecov](https://codecov.io/gh/pulse-live/pulse/graph/badge.svg?token=l4kknNVRYG)](https://codecov.io/gh/pulse-live/pulse)
[![License: BSD-3-Clause](https://img.shields.io/badge/License-BSD--3--Clause-blue.svg)](LICENSE)

# Pulse

A lightweight, autonomous monitoring agent written in Rust using Tokio.

Pulse runs on public monitoring servers and customer-owned infrastructure. It is
BSD-3-Clause licensed and works without a Pulse.live account or subscription.
[Pulse Cloud](https://pulse.live) is an optional, separately maintained control
plane. Compatible control planes can implement the public protocol.

## Status

This version implements a working monitoring slice:

- HTTP/HTTPS GET and HEAD, status validation, bounded redirects and body inspection.
- Autonomous recurring checks with phase offsets, stable jitter and missed-tick skipping.
- Shared concurrency, connection, start-rate and payload limits for routine and Live Checks.
- Explicit HTTPS enrollment, credential renewal, conditional plan synchronization and offline leases.
- Bounded SQLite observation buffering, Protobuf batches and durable acknowledgements.
- Long-poll control notifications, correlated Live Checks and local community opt-in.
- JSON-lines local observations, JSON status and optional OpenTelemetry diagnostics.

The protocol is a **v1 draft**, tested against a compatible local HTTPS fixture.
This is not a production-readiness claim. DNS/TCP/TLS/ICMP probe engines, host
collectors, additional exporters and a community credit system remain deferred.
See [architecture and rollout decisions](docs/ARCHITECTURE.md).

## Installation

Release workflows produce `.tar.gz`, `.deb` and `.rpm` files for x86_64 and ARM64,
with SHA-256 checksums. The standard `pulse` packages have diagnostic telemetry
**disabled at compile time**. Separately named `pulse-telemetry` packages enable
that optional capability; exporting still requires an explicit OTLP endpoint.
Install one variant at a time because they share `/usr/bin/pulse`.

For an x86_64 version 0.1.0 release, after verifying its checksum:

```sh
sudo apt install ./pulse_0.1.0-1_amd64.deb
# On an RPM-based distribution:
sudo dnf install ./pulse-0.1.0-1.x86_64.rpm
```

Packages install the CLI, license, documentation and examples under
`/usr/share/doc/pulse/`. Monitoring starts only when explicitly run; installation
creates no enrollment identity or running service. ARM64 packages use Debian's
`arm64` and RPM's `aarch64` architecture labels. See
[CI and releases](docs/RELEASING.md) for all formats and variants.

## Build from source

Rust 1.88 or newer is required. Building does not start monitoring or send data.

```sh
cargo build --release --locked
cargo build --release --features telemetry --locked
```

The executable is `target/release/pulse`. Put it on your `PATH` for these examples.

## Standalone monitoring

```sh
pulse --help
pulse -V
pulse --version
pulse config validate --file examples/pulse.toml
pulse run --config examples/pulse.toml
```

The example monitors two public HTTPS endpoints and explicitly enables JSON lines
on stdout. Stop with SIGINT or SIGTERM. Configuration validation performs no DNS
lookups or connectivity checks. `-V` prints the package version; `--version` also
prints the build-time Git hash, or `unknown`.

A minimal running configuration is:

```toml
[agent]
mode = "standalone"
state_dir = ".pulse"

[output]
stdout = true

[[probes]]
id = "homepage"
kind = "http"
target = "https://example.com"
interval_ms = 300000
timeout_ms = 5000
phase_offset_ms = 0
```

Intervals are milliseconds. An omitted phase is spread deterministically by check
ID; explicit zero is respected. The local state directory must be owned by the
running user, mode `0700`, on a local filesystem. Pulse creates it when absent.
Only one executor may use a state directory at a time.

Private targets require `[network] allow_private_networks = true`. Link-local and
metadata destinations remain forbidden. Public-edge and community identities
cannot enable private probing. Custom trust roots can be supplied with
`network.ca_file`; certificate verification remains enabled.

## Managed operation

Use the same state directory for enrollment and execution:

```sh
# Supply PULSE_TOKEN through your secret manager/environment.
pulse enroll --server https://api.pulse.live --state-dir .pulse/edge
pulse config validate --file examples/managed-edge.toml
pulse run --config examples/managed-edge.toml
pulse status --state-dir .pulse/edge
```

Enrollment reads `PULSE_TOKEN`, or accepts `--token`. Environment delivery avoids
putting the token in shell history or process arguments. Credentials are stored in
private state files, never printed. Custom control-plane CAs use `--ca-file` during
enrollment and `agent.ca_file` during execution. Destinations are configurable.

Cloud assigns checks; the agent executes them locally without per-check permission
requests. Accepted plans remain active during outages until their lease expires.
Unacknowledged observations are retried with stable IDs. A `304` never renews a
lease. Cloud's proposed three-location/five-minute policy is orchestration policy,
not an agent restriction.

Community participation requires enrollment and is disabled by default:

```sh
pulse community enable --state-dir .pulse/edge
pulse community status --state-dir .pulse/edge
pulse community disable --state-dir .pulse/edge
```

The agent checks local opt-out once per second and cancels community work. Community
work uses public targets and stricter local capacity; it cannot access host metrics
or monitoring credentials. Enrollment establishes deployment identity through the
control plane; there is no user-selected managed-edge role.

## CLI and diagnostics

```text
pulse run --config <FILE>
pulse probe http <URL> [--allow-private]
pulse config validate [--file <FILE>]   # defaults to pulse.toml
pulse enroll --server <HTTPS_URL> [--token <TOKEN>] [--state-dir <DIR>] [--ca-file <PEM>]
pulse status [--state-dir <DIR>]
pulse community <enable|status|disable> [--state-dir <DIR>]
```

One-shot probing returns a real observation as JSON and exits nonzero for outcomes
other than PASS. Future protocol engines have no misleading command stubs.
Results go to stdout; diagnostics go to stderr. Status is a local snapshot and
includes execution phase, counters and queued records, without credential values.

Global `-v`, `-vv`, `-vvv` select INFO, DEBUG, TRACE; the default is ERROR.
`RUST_LOG` overrides this fallback and invalid filters fail clearly. `NO_COLOR`
disables color; explicit `--color` overrides it. `--no-color` disables color and
conflicts with `--color`. TOML `[logging]` is reserved; current diagnostics use
CLI options and environment variables.

Local logging works in every build. With `--features telemetry`, diagnostic export
requires explicit configuration:

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=https://collector.example:4317 \
  OTEL_EXPORTER_OTLP_PROTOCOL=grpc RUST_LOG=pulse=info \
  pulse run --config examples/pulse.toml
```

This optional OTLP integration uses gRPC over verified HTTPS, with plaintext allowed
only for loopback collectors. It is separate from the native observation protocol,
which uses HTTPS requests with JSON control and Protobuf batches. Disabling the
Cargo feature never disables monitoring. Help/version do not initialize exporters.
See [CLI architecture](docs/CLI_ARCHITECTURE.md) for exporter configuration.

## Resource use and security

Local policy always limits remote assignments. Admission happens before spawning
probe tasks; queues, response inspection and persistent storage are bounded.
Overload skips work and records counters instead of accumulating catch-up jobs.
Local stdout is best-effort; managed observations are retained until durable ack,
subject to the configured spool capacity and disk health.

Connection and memory reservations are application budgets, not whole-process OS
quotas. Payload limits do not account for every TLS/header byte. Apply systemd or
container CPU, memory and file-descriptor limits alongside agent limits when
running near routing services. Run as an unprivileged user with private writable
state and a read-only configuration. No listening service is required.

Pulse verifies TLS, pins validated DNS answers to connections, rechecks redirects,
and rejects credentials in target URLs. It ignores ambient proxies. Installing
Pulse sends nothing. Running probes, enrollment and diagnostic export each require
explicit configuration or commands. There is no remote shell, subscription check,
automatic host collection, or proprietary Cloud dependency. Never place secrets in
probe targets, logs or example files. Report vulnerabilities privately using the
[security policy](.github/SECURITY.md).

## Development

```sh
just ci         # fmt, default/all-feature checks, strict Clippy, both test suites
just coverage   # requires cargo-llvm-cov and rustup LLVM tools
just workflow-test # release-gate tests; requires Python 3.11+ and PyYAML
just measure    # Linux/Python 3 local load, overload and timeout measurements
cargo +1.88.0 check --all-targets --all-features --locked
cargo test --test managed --locked
```

Integration tests start a local HTTPS control plane with an explicit test CA; they
need no external websites or Pulse Cloud. They exercise enrollment, autonomous
execution, Live Checks, outages, restart recovery, revision replacement, lease
expiration, lost acknowledgements, TLS rejection and graceful shutdown.
[Local resource measurements](docs/PERFORMANCE.md) record CPU, RSS, descriptors
and schedule delay under three workloads.

Read [AGENTS.md](AGENTS.md), [architecture](docs/ARCHITECTURE.md), and the
[public protocol draft](docs/PROTOCOL.md) before extending an execution path.
Cloud's dashboard, organizations, alerts, billing and fleet-wide incident decisions
remain in the separate [Cloud repository](https://github.com/pulse-live/cloud).
See [CI and releases](docs/RELEASING.md) for workflow triggers, optional Codecov
configuration, Linux archives and version-tag releases.

## License

[BSD-3-Clause](LICENSE).
