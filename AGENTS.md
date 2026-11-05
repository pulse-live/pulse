# AGENTS.md

These guidelines apply to contributors and coding agents throughout this repository.
Keep changes focused, preserve security invariants, and prioritize predictable
resource use on hosts whose primary workload is not monitoring.

## Project scope

Pulse is a single Rust package for an open-source monitoring agent. Preserve the
`pulse` package name, repository identity, author metadata, and BSD-3-Clause
[LICENSE](LICENSE). Use Rust 2024 and maintain the Rust 1.88 MSRV.

The same binary is intended for public edge servers and customer-owned private
infrastructure. Standalone use must remain independent of accounts, subscriptions,
and proprietary services. Pulse Cloud is an optional control plane maintained in
another repository; dashboard, billing, and centralized incident decisions belong
there. Keep destinations configurable and the agent protocol publicly implementable.

HTTP/HTTPS monitoring, local scheduling, resource admission, typed observations,
HTTPS enrollment/control, bounded SQLite buffering, durable acknowledgements and
Live Checks are implemented as a tested vertical slice. Community participation
is explicit and locally bounded. The public v1 protocol remains a draft. Collectors,
other probe engines and production fleet hardening remain deferred. Do not expose
commands without real semantics or fabricate measurements.

## Before changing code

- Read [README.md](README.md), [Cargo.toml](Cargo.toml), the affected implementation,
  and [.justfile](.justfile). Inspect existing changes before editing.
- Consult [CLI_ARCHITECTURE.md](docs/CLI_ARCHITECTURE.md),
  [ARCHITECTURE.md](docs/ARCHITECTURE.md), and [PROTOCOL.md](docs/PROTOCOL.md)
  for ownership boundaries and planned behavior.
- Fix shared problems in their responsible component rather than duplicating
  caller-specific patches. Preserve behavior outside the requested scope.
- Keep one package. Introduce modules and public interfaces when they serve an
  implemented use case; avoid speculative frameworks, wrappers, or empty trees.

## Architecture and ownership

Preserve the clap v4 builder API and this flow:

```text
commands -> ArgMatches -> dispatch -> Action -> execution
```

| Location | Responsibility |
| --- | --- |
| `src/bin/pulse.rs` | Thin Tokio current-thread entry point, execution, shutdown, exit status |
| `src/lib.rs` | Reusable agent, CLI, configuration and protocol modules |
| `src/cli/commands/` | Argument definitions, help, styles, and version metadata; no business I/O |
| `src/cli/dispatch/` | Pure validation and strongly typed action construction; no DNS or network calls |
| `src/cli/actions/` | Execution and calls into the responsible domain component |
| `src/cli/start.rs` | Parsing, verbosity, tracing initialization, and dispatch |
| `src/cli/telemetry.rs` | Local logging and optional diagnostic exporter lifetime |
| `src/config/` | Typed TOML parsing, local safety limits, and shared target validation |

Keep collectors independent of probe execution. The control plane selects what
checks to run; the local scheduler decides when they execute within local
budgets. Extend engines and collectors incrementally as described in the architecture
documents. Preserve the existing scheduler, governor, storage and transport ownership.
The public protocol remains an unstable draft.

## Performance and resource governance

- Bound work before creating tasks, opening connections, or allocating buffers.
  A semaphore inside an already unbounded set of spawned tasks is insufficient.
- Define limits in both count and bytes for queues, batches, results, and caches.
  Reject, skip, coalesce, or backpressure excess work with explicit behavior.
- Keep active task, socket, and buffer usage proportional to configured concurrency.
  Any future connection reuse or cache needs bounded capacity and idle expiry;
  do not retain resources for every discovered target indefinitely.
- Avoid repeated parsing, unnecessary cloning, intermediate collections, and string
  formatting in hot paths. Prefer borrowing and reuse when they keep ownership clear.
  Do not trade readable code for an unmeasured micro-optimization.
- Keep blocking I/O and sustained CPU work off the async executor. Bound submissions
  to `spawn_blocking`; do not create a background task for every small operation.
- Avoid holding locks across `.await`. Keep critical sections short, and prefer
  ownership or message passing over shared mutable state when practical.
- Recurring work must use stable jitter and skip or coalesce missed ticks.
  Do not create catch-up storms, overlapping executions for one check, busy loops,
  or unlimited retry queues. Cap retries and apply backoff with jitter.
- Carry cancellation and an overall deadline through every network phase. Release
  permits and resources on success, failure, timeout, and cancellation.
- Use monotonic clocks for durations. Keep scheduler delay separate from endpoint
  latency and represent skipped phases as absent values rather than zeroes.
- Enforce input and response byte caps while reading, before unbounded buffering.
  The current configuration reader has a 1 MiB limit; preserve it.
- Treat logging and diagnostic export as workloads with their own budgets. Avoid
  high-cardinality fields, large payloads, and per-iteration informational logs.
- Support performance claims with representative local measurements. For changes
  to execution paths, compare relevant CPU, allocations, memory, connections, and
  latency under normal load, overload, and failure. Do not infer performance from
  code shape alone or introduce benchmark infrastructure without a concrete need.

Local safety ceilings must always win over remote assignments. Protect routing and
other primary workloads during control-plane outages as well as normal operation.
Admission and payload/storage limits are enforced in the executor. Connection and
memory reservations are application budgets, not OS-wide FD/RSS/bandwidth quotas;
keep these distinctions explicit in documentation and performance claims.

## Rust style and lint contract

- Use explicit types, focused functions, and modules with one responsibility.
  Prefer concrete types or enums over unnecessary dynamic dispatch. If async traits
  become necessary, prefer native `async fn` over adding an adapter dependency.
- Use `snake_case` for modules/functions, `UpperCamelCase` for types, and
  `SCREAMING_SNAKE_CASE` for constants. Group related imports and follow rustfmt.
- Prefer `?`, useful error context, and typed errors where callers need to classify
  failures. Do not introduce `unwrap()`, `expect()`, panics, or unchecked indexing.
  Guard arithmetic bounds and zero divisors where applicable.
- Rust warnings are denied and unsafe code is forbidden. Preserve strict Clippy
  groups and explicit safety/performance lints in the manifest. Fix the code rather
  than weakening lint settings or suppressing complexity warnings.
- Keep any justified lint exception local and documented. The existing generated
  build-documentation allowance is not permission to suppress handwritten code.
- Use file size as a signal to review responsibilities, not a quota. Avoid both
  oversized mixed modules and unnecessary tiny forwarding modules.
- Keep dependencies and enabled features minimal, default features empty, and
  `Cargo.lock` tracked. Verify MSRV and both feature sets when dependencies change.
- Preserve the release profile unless measurements justify a change. Keep generated
  `built.rs` private and unedited; cache version formatting once per process.

## Validation and security boundaries

- Keep configuration defaults in their owning types and CLI definitions in their
  builders. Reuse validation instead of maintaining inconsistent copies.
- Reject unknown TOML fields, invalid targets, duplicate IDs, incompatible modes,
  unsafe numeric limits, and timeouts beyond local ceilings. Revalidate
  remote assignments before replacing accepted configuration.
- Standalone mode rejects a control-plane URL. Managed mode requires an
  explicit HTTPS destination and rejects local probe definitions.
- Preserve URL credential/fragment restrictions, DNS and port validation, and the
  distinction between syntactic validation and connectivity checks.
- Treat remote configuration and on-demand requests as untrusted. Do not add
  arbitrary command execution, automatic host collection/upload, or paid-feature checks.
- Never log secrets, credentials, raw configuration, or sensitive request data.
  Error context and tracing attributes must preserve the same redaction boundary.
- Verify TLS certificates and hostnames. Do not disable verification to make a test
  or integration pass. Keep installation and standalone validation network-free
  unless diagnostic export is explicitly configured.

## CLI and diagnostic contracts

- Use clap's help/version handling. Keep the existing help palette and global color
  options, including `NO_COLOR`, explicit overrides, and conflicting color flags.
- `-V` prints the package version; `--version` includes the build-time Git hash or
  `unknown`. Build metadata comes from `built` with `git2`, not runtime Git commands.
- Diagnostics go to stderr and command results to stdout. Verbosity maps to ERROR
  by default, INFO for `-v`, DEBUG for `-vv`, and TRACE for `-vvv` or more.
  `RUST_LOG` overrides the fallback; invalid filters fail clearly.
- Local logging works in every build. The default-off `telemetry` feature controls
  agent diagnostic tracing, never the native probe-result or future host-metric pipeline.
- Create an OTLP exporter only with explicit `OTEL_EXPORTER_OTLP_ENDPOINT` setup.
  Support gRPC over HTTPS with WebPKI roots; plaintext is limited to loopback.
  Preserve endpoint validation and keep exporter queues and batches bounded.
- The caller owns the provider. Flush explicitly, always attempt shutdown even if
  flushing fails, and run blocking provider operations off the runtime thread while
  Tokio remains alive. Surface failures; do not imply that a timeout means delivery.
- Keep [examples/pulse.toml](examples/pulse.toml) valid and credential-free. Logging
  fields in TOML remain reserved; current diagnostics use flags/env.

## Development and verification

```sh
just fmt         # Format sources
just fmt-check   # Check formatting
just check       # Check default and all-feature builds
just clippy      # Strict lints for every target and feature
just test        # Default and all-feature test suites
just ci          # Formatting, checks, Clippy, and tests
just coverage    # All-feature coverage summary
just workflow-test # Release validation tests (Python 3.11+ and PyYAML)
just measure     # Local Linux load/overload/timeout measurements
```

Coverage requires `cargo-llvm-cov` and rustup LLVM tools. There is no coverage
threshold; use coverage to find meaningful gaps, not to justify shallow tests.
The reusable test workflow also verifies the MSRV:

```sh
cargo +1.88.0 check --all-targets --all-features --locked
```

For code changes, run the relevant checks and the full `just ci` flow before
completion. Check MSRV when changing dependencies or using newer Rust APIs. For
CLI/configuration changes, exercise the affected binary commands as well.
Documentation-only changes need link/accuracy checks and `git diff --check`, not
an unchanged Rust test run. Report actual commands, results, and limitations.

## Testing and documentation

- Keep tests deterministic and independent of live Cloud services or external
  websites. Use local fixtures and subprocesses; keep unit tests near their code
  and public behavior tests under `tests/`.
- Add regression coverage for behavior bugs. Test relevant malformed, missing,
  oversized, duplicate, incompatible, timeout, and cancellation cases. Assert the
  actual rejection or resource release rather than only a generic error.
- Do not count skipped tests, empty filters, failed setup, or swallowed assertions
  as successful validation. Use small test doubles only where they verify a real boundary.
- Isolate environment variables on child processes rather than mutating the test
  runner's environment. Preserve the tracing lifecycle test that depends on an
  active current-thread runtime during flush and shutdown.
- Update existing documentation when behavior, configuration, or commands change.
  Explain module flow, ownership, resource budgets, and trust boundaries with `//!`;
  keep `///` focused on intent, invariants, side effects, and errors.
- Document non-obvious validation, fallback precedence, cancellation, and failure
  handling. Avoid repeating implementation details or historical task logs.
- Keep README claims accurate about implemented versus planned features. Detailed
  configuration ranges and protocol design belong in the existing architecture docs.

## Autonomous execution invariants

- Keep one bounded heap entry per assignment and admit before spawning. Preserve
  monotonic epoch/phase scheduling, stable jitter, missed-cycle skipping and
  cancellation. Cloud assigns work; it never authorizes each recurring execution.
- Validate complete snapshots before durable replacement. Never renew a plan lease
  through 304, events, heartbeats or credential rotation. Preserve rollback checks.
- Enrolled deployment identity comes from trusted provisioning. Public-edge and
  community work cannot use private targets even with local private permission.
  Pin validated DNS answers to actual connections and revalidate every redirect.
- Reserve command IDs before claims and execution. Do not re-execute uncertain
  commands after restart; preserve command/run correlation and result IDs. All
  Live Checks share the same governor and community policy as recurring work.
- Keep native Protobuf observations independent of optional OTLP diagnostics.
  Delete only explicitly acknowledged durable IDs. Retry stable IDs; preserve
  bounded spool accounting, newest-drop policy and loss counters.
- SQLite work stays serialized off the executor. Preserve owner-only files, local
  locking, transaction boundaries and the database/journal storage headroom. Local
  status, community controls and credential recovery must use existing metadata
  without reapplying default spool limits to a larger configured database.
- Bound output and shutdown drains. Preserve the blocked-pipe SIGTERM regression
  test and cancellation of every owned control/probe task. Document OS blocking
  DNS/filesystem limitations rather than claiming unqualified shutdown guarantees.

## CI and release maintenance

- Keep tests, coverage, dependency audit and native release builds in their existing
  reusable workflows. Update [RELEASING.md](docs/RELEASING.md) when triggers, assets
  or release behavior change; avoid duplicate CI entry points. Run
  `just workflow-test` when changing release validation or artifact names.
- Pin external actions to reviewed commit SHAs with upstream ref comments. Keep
  repository permissions read-only outside the publication job and disable
  persisted checkout credentials. Optional Codecov secrets belong only to uploads.
- Release only a version-matched tag in main history. Pass the verified commit SHA
  to every reused workflow and require all checks, both architectures and both
  feature sets before publishing checksummed archives, DEB and RPM packages. Keep
  version tags immutable.
- Preserve [the security reporting policy](.github/SECURITY.md), the private report
  contact and the distinction between this agent and Pulse Cloud.
- Keep standard native packages telemetry-free; use the explicitly named telemetry
  variants for diagnostic export capability. Preserve package conflicts and static
  binary/payload checks. Packaging must not install activation scripts, credentials
  or automatically running monitoring services.
