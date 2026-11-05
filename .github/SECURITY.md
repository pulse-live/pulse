# Security policy

## Supported versions

Pulse is in initial development. Security fixes are made on `main` and in the
latest published 0.1.x release when available. Older release lines do not receive
backported fixes. The draft agent-control protocol is not yet a stable production
compatibility contract.

## Report a vulnerability

Do not publish vulnerabilities, credentials or private monitoring targets in
GitHub issues or pull requests. Email the Pulse team at
[team@pulse.live](mailto:team@pulse.live)
with the subject `Pulse security report` and include:

- Affected version and Git commit, operating system and enabled Cargo features.
- Expected behavior, actual behavior and potential impact.
- Minimal reproduction steps or a proof of concept using systems you control.
- Relevant redacted configuration and diagnostics, and any proposed mitigation.

Useful reports include private-address or metadata access bypasses, credential
exposure, authorization/lease bypasses, TLS verification failures, unbounded work
or storage growth, and unsafe handling of control-plane input.

Remove enrollment tokens, bearer credentials, OTLP headers and private endpoint
details from attachments. Do not upload the state database: it contains credentials.
Do not test against unrelated endpoints or production routing infrastructure.

The Pulse team will assess the report and coordinate mitigation and disclosure
with you. Please allow time for a fix before publishing exploit details. Response
and remediation times depend on severity and maintainer availability.

## Scope and deployment

This policy covers the open-source Pulse agent. Pulse Cloud is maintained
separately; this repository does not contain its dashboard, billing or backend.

Run Pulse without root privileges, restrict state directories to their owner,
and apply operating-system CPU, memory and file-descriptor limits alongside the
agent's local budgets. Private probing, community participation and diagnostic
export require explicit local authorization/configuration. See the
[architecture and rollout decisions](../docs/ARCHITECTURE.md) for current limits.
