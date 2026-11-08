# Show available development commands.
default:
    @just --list

# Format Rust source files.
fmt:
    cargo fmt --all

# Verify formatting without modifying files.
fmt-check:
    cargo fmt --all -- --check

# Check default and telemetry-enabled builds.
check:
    cargo check --locked
    cargo check --all-features --locked

# Run strict Clippy checks for all targets with both feature configurations.
clippy:
    cargo clippy --all-targets --locked
    cargo clippy --all-targets --all-features --locked

# Run tests with and without optional telemetry.
test:
    cargo test --locked
    cargo test --all-features --locked

# Run the standard local CI checks.
ci: fmt-check check clippy test

# The operator bumps the version beforehand, as the last commit on sandbox, and
# fast-forwards develop to it once Test & Build is green. See docs/RELEASING.md.
# Release develop: start the release workflow, follow it, then fast-forward main.
release:
    #!/usr/bin/env bash
    set -euo pipefail
    git fetch --quiet origin develop
    sha="$(git rev-parse origin/develop)"
    last="$(gh run list --workflow release.yml --branch develop --event workflow_dispatch --limit 1 --json databaseId --jq '.[0].databaseId // 0')"
    gh workflow run release.yml --ref develop
    # The dispatched run is the first new one for this commit; never just "the latest".
    run=""
    for _ in $(seq 30); do
        run="$(gh run list --workflow release.yml --branch develop --event workflow_dispatch --limit 10 --json databaseId,headSha --jq "[.[] | select(.databaseId > ${last} and .headSha == \"${sha}\")][0].databaseId // empty")"
        [[ -z "${run}" ]] || break
        sleep 2
    done
    [[ -n "${run}" ]] || { echo "The dispatched release run did not appear." >&2; exit 1; }
    gh run watch "${run}" --exit-status
    # develop's own Test & Build must have finished, and passed, before main can move.
    ci=""
    for _ in $(seq 30); do
        ci="$(gh run list --workflow build.yml --branch develop --commit "${sha}" --limit 1 --json databaseId --jq '.[0].databaseId // empty')"
        [[ -z "${ci}" ]] || break
        sleep 2
    done
    [[ -n "${ci}" ]] || { echo "No Test & Build run of develop for ${sha}; push it to main yourself when CI OK passed." >&2; exit 1; }
    echo "Waiting for develop's Test & Build (run ${ci}) before moving main..."
    gh run watch "${ci}" --exit-status > /dev/null || { echo "Test & Build failed on ${sha}; re-run its failed jobs, then: git push origin ${sha}:refs/heads/main" >&2; exit 1; }
    git push origin "${sha}:refs/heads/main"

# Print basic coverage; requires cargo-llvm-cov and the rustup LLVM tools component.
coverage:
    cargo llvm-cov --all-features --locked

# Measure a local Linux process under ordinary load, overload, and timeouts (Python 3).
measure:
    cargo build --release --locked
    python3 scripts/measure-agent.py
