# CI and releases

Pulse uses reusable GitHub Actions workflows with read-only repository access.
Only the final GitHub release job receives `contents: write`. External actions are
pinned to commit SHAs; version comments identify the corresponding upstream refs.
Build is the single pull-request entry point, so tests, coverage and audit run once
while every architecture/feature combination also exercises native packaging.
Release-gate regressions run through `just workflow-test` (Python 3.11+ and
PyYAML), using isolated Git and artifact fixtures. They cover tag/version checks,
main ancestry, prereleases, complete artifact sets and malformed checksums.

| Workflow | Trigger and responsibility |
| --- | --- |
| `test.yml` | Manual runs or reuse: formatting, both feature sets, strict Clippy, tests and Rust 1.88 |
| `coverage.yml` | Manual runs or reuse: all-feature LCOV report and a retained coverage artifact |
| `security-audit.yml` | Daily, manual runs, or reuse: audit the tracked lockfile |
| `build.yml` | Pull requests, pushes to main, manual runs, or reuse: invoke tests/coverage/audit and build native Linux release files |
| `release.yml` | Version tag pushes or a manual existing-tag selection: verify the source, build and publish release files |

Coverage uses `cargo-llvm-cov`, matching `just coverage`. Its LCOV artifact is
available for 14 days without external service credentials. To also upload to
Codecov, configure the optional repository secret `CODECOV_TOKEN`; only the upload
step receives it. Fork pull requests without that secret still produce coverage.
There is no coverage percentage threshold. The dependency audit fails on known
vulnerabilities and advisory warnings, including optional and development packages.

## Linux artifacts

Native x86_64 and ARM64 runners build musl binaries for:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`

Each target produces a standard binary with telemetry disabled and a separate
opt-in telemetry binary. Each binary is packaged as `.tar.gz`, `.deb` and `.rpm`,
for **12 release files** in total. All use the locked dependencies and release
profile. Both feature sets are smoke-tested with help, short/long version and
validation of both example configurations. Native package payloads are extracted
and checked against the original executable before checksums are generated.
Ordinary main builds retain all files and individual SHA-256 files for 14 days.

| Rust target | Debian architecture | RPM architecture |
| --- | --- | --- |
| `x86_64-unknown-linux-musl` | `amd64` | `x86_64` |
| `aarch64-unknown-linux-musl` | `arm64` | `aarch64` |

| Format | Standard build | Opt-in diagnostic telemetry |
| --- | --- | --- |
| Archive | `pulse-0.1.0-x86_64-unknown-linux-musl.tar.gz` | `pulse-0.1.0-x86_64-unknown-linux-musl-telemetry.tar.gz` |
| Debian | `pulse_0.1.0-1_amd64.deb` | `pulse-telemetry_0.1.0-1_amd64.deb` |
| RPM | `pulse-0.1.0-1.x86_64.rpm` | `pulse-telemetry-0.1.0-1.x86_64.rpm` |

The same three formats are generated for ARM64. Default Cargo features are empty;
standard packages are named `pulse`. The `pulse-telemetry` packages are explicitly
selected alternatives, declare conflicts with the standard package and provide
the same `/usr/bin/pulse`. Diagnostic export remains disabled until an OTLP
endpoint is configured, even in the telemetry variant. Native package metadata is
in Cargo.toml; packaging tools are pinned in the build workflow.

Native packages install only the executable, license, README, security policy and
credential-free examples. They contain no installation scripts, service units,
enrollment credentials or active monitoring configuration. Installation never
starts a service or sends a probe. Examples are in `/usr/share/doc/pulse/examples`;
select and copy/edit a configuration before running the agent. The release helper
rejects a mismatched version, architecture, dynamic binary or feature selection.
This metadata is intended for the verified static musl release path.

Archives also contain the architecture/protocol documentation, public Protobuf
schema and contributor guidelines. The release publishes a combined `SHA256SUMS`
covering all 12 files. With all release files downloaded, run:

```sh
sha256sum --check SHA256SUMS
```

For a single downloaded file, select its matching checksum line. Native package
versions map Cargo prerelease hyphens to `~`, so `0.1.0-rc.1` becomes
`0.1.0~rc.1-1` in DEB and `0.1.0~rc.1` with release `1` in RPM; prereleases sort
before stable versions. Archive names retain the original Cargo version.

## Package locally

Use a native Linux host for the target architecture with musl, binutils, Python
3.11+, `dpkg-deb`, RPM query/extraction tools and cpio installed. Install the same
packager versions as CI:

```sh
cargo install cargo-deb --version 3.8.0 --locked
cargo install cargo-generate-rpm --version 0.21.1 --locked
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --no-default-features --target x86_64-unknown-linux-musl
bash scripts/package-release.sh x86_64-unknown-linux-musl
```

Output defaults to `dist/`. Telemetry is opt-in for both compilation and packaging:

```sh
cargo build --release --locked --no-default-features --features telemetry --target x86_64-unknown-linux-musl
bash scripts/package-release.sh x86_64-unknown-linux-musl telemetry
```

The optional third script argument selects an output directory. The helper reuses
the built binary with `cargo deb --no-build` and `cargo generate-rpm`; it never
rebuilds with a different feature set. Packages are unsigned local-install files;
package signing, APT/DNF repository hosting and container distribution are separate
work. Verify release checksums before installation.

## Create a release

1. Update `package.version` in Cargo.toml and refresh Cargo.lock with `cargo check`.
2. Run `just ci`, the MSRV check and `cargo audit --deny warnings`. Review changes
   to the public protocol and deployment limitations before tagging.
3. Merge the reviewed version commit into main. Create and push its version tag:

   ```sh
   git tag -a v0.1.0 -m "Pulse 0.1.0"
   git push origin v0.1.0
   ```

The workflow also accepts `0.1.0` without the `v` prefix. Tags must match the
manifest version exactly and resolve to a commit in main's history. Prerelease
tags such as `v0.1.0-rc.1` require the same prerelease version in Cargo.toml and
create a GitHub prerelease. Build metadata suffixes (`+...`) are not accepted.

A manual run takes an existing tag; it does not create a new one:

```sh
gh workflow run release.yml --ref main -f tag=v0.1.0
```

Verification resolves annotated tags to a commit and passes that exact SHA into
every reused workflow. Publication waits for tests, coverage, audit and all four
builds, validates that each checksum record names its corresponding file, verifies
every digest and requires all 12 files across formats, architectures and feature
sets.
The publication step leaves existing assets unchanged (`overwrite_files: false`).
Treat version tags and published assets as immutable and use a new version when
rebuilding changed code or dependencies.

Protect main with required status checks and protect version tags against
unauthorized creation, movement and deletion. The workflow definitions themselves
are release code and should receive the same review as Rust changes. GitHub-hosted
runner execution and repository protections are configured in GitHub; local checks
cannot establish those settings.
