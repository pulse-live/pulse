# CLI architecture

Pulse uses clap v4 builders and Cargo metadata within one Rust package. Preserve
this ownership flow when adding commands:

```text
commands -> ArgMatches -> dispatch -> Action -> execution
```

`commands/mod.rs` defines arguments, examples, styles and version metadata without
business I/O. Helpers share repeated definitions. The probe hierarchy currently
contains HTTP; add an engine before exposing another protocol command.

`dispatch/mod.rs` performs pure syntactic validation and constructs typed actions.
It never reads configuration, resolves DNS, loads credentials or opens connections.
Files become paths and secrets use a Debug-redacted type.

`actions/mod.rs` routes Run, Probe, Enroll, ConfigValidate, Status and Community to
execution modules. Run loads validated configuration and installs SIGINT/SIGTERM
cancellation. Domain work belongs to the agent, probe, transport or storage owner.
Config validation reads at most 1 MiB and performs no connectivity checks. Status
and community operations use local state; enrollment is an explicit HTTPS exchange.

`start.rs` parses arguments, determines verbosity, initializes diagnostics and
returns the dispatched Action. The thin `src/bin/pulse.rs` executes it in a Tokio
current-thread runtime, then attempts diagnostic shutdown even after an error.
Clap help/version/syntax handling happens before diagnostic initialization.

## Presentation and results

Help uses bold yellow headers, green usage/placeholders and bold blue literals.
`NO_COLOR` disables color, explicit global flags override it, and conflicting
`--color`/`--no-color` flags fail. Scanning stops at `--`; otherwise terminal
detection applies. These flags work at nested command positions.

`-V` prints the package version. `--version` also prints the build-time Git hash
or `unknown`. `build.rs` uses `built` with `git2`; generated metadata is private
and long-version formatting is cached once. No runtime Git command is executed.

Results are stdout JSON or JSON lines, except human-readable config validation.
Diagnostics go to stderr. One-shot probes return exit 1 for any non-PASS result.
Unsupported commands fail through clap, without fake results or network access.

## Diagnostic lifetime

`telemetry.rs` owns local tracing and optional diagnostic export. Fallback levels
are ERROR with no verbosity, INFO with `-v`, DEBUG with `-vv`, TRACE with `-vvv` or
more. `RUST_LOG` overrides this and invalid filters fail. Filter regex is disabled.
TOML logging settings are reserved; they do not override CLI/environment settings.

The default-off `telemetry` Cargo feature adds OTLP diagnostic capability only.
Local logging always works. An exporter is created only when the generic
`OTEL_EXPORTER_OTLP_ENDPOINT` is explicitly set. It must be an HTTPS root URL
without credentials/query/fragment; HTTP is allowed only on loopback. Transport is
gRPC with WebPKI roots. Protocol variables must be `grpc`. Signal-specific endpoint
variables do not enable or redirect export. The SDK supports generic and
trace-specific OTLP header variables. Custom diagnostic CA/client certificates and
HTTP OTLP are not configured by this implementation.

The caller owns the provider. The bounded batch processor is flushed explicitly;
shutdown is attempted even after flush failure. Both blocking operations run in
`spawn_blocking` while Tokio continues driving transport I/O. Failures are visible;
a timeout does not imply delivery. The feature never controls native observations,
which use the independent HTTPS/Protobuf pipeline.

## Extending behavior

Add builders, a typed Action, pure dispatch and domain implementation in that
order. Test parsing, missing/invalid input, output and failures with local fixtures.
Keep module documentation focused on ownership, deadlines and trust boundaries.
Do not create commands with successful no-op behavior or start workers during
argument parsing. Full CLI/configuration/diagnostic behavior is tested without
external targets.
