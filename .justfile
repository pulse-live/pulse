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

# Test release workflow gates; requires Python 3.11+ and PyYAML.
workflow-test:
    python3 tests/release_workflows.py

# Print basic coverage; requires cargo-llvm-cov and the rustup LLVM tools component.
coverage:
    cargo llvm-cov --all-features --locked

# Measure a local Linux process under ordinary load, overload, and timeouts (Python 3).
measure:
    cargo build --release --locked
    python3 scripts/measure-agent.py
