# Public Agent–Control Plane Protocol v1 draft

This is an **unstable, implemented draft**, not a production compatibility promise.
The public Rust types and [Protobuf schema](../proto/pulse/v1/observations.proto)
are sufficient to implement a compatible control plane without proprietary code.
[tests/managed.rs](../tests/managed.rs) is a local HTTPS reference fixture, not a
production server. Version negotiation and rollout require review before freezing v1.

## Transport, trust and limits

All agent-control traffic is agent-initiated HTTPS to a configured root origin.
HTTP/2 is supported with HTTP/1.1 compatibility. There is no broker, WebSocket,
gRPC control API or manifest object store. TLS certificates and hostnames are
verified with system-independent WebPKI roots or an explicitly configured PEM CA.
Redirects and ambient proxies are disabled. The origin cannot contain credentials,
a query, fragment or non-root path. Credentials are bound to that canonical origin.

Except enrollment, requests use `Authorization: Bearer <scoped-agent-token>`.
Authentication establishes agent identity, tenant and assignment permissions;
request parameters cannot select another identity. A control plane must scope
credentials to these operations and prohibit arbitrary remote code execution.
Deployment roles (`private`, `public_edge`, `community`) are server-provisioned.
Enrollment accepts no caller-selected role or tenant claim.

Control JSON uses strict typed objects and rejects unknown fields. Timestamps are
unsigned UTC Unix milliseconds; intervals and offsets are integer milliseconds.
Targets and control URLs are at most 2,048 bytes. IDs contain 1..64 ASCII letters, digits, `_` or `-`. Revision/epoch integers are
64-bit; browser implementations must preserve integers beyond JavaScript's safe
number range or negotiate a future schema change. `schema_version` is 1 wherever
present. Additions to strict JSON require coordinated capability/version handling;
Protobuf field numbers must never be reused.

| Exchange | Success | Response limit | Client deadline |
| --- | --- | --- | --- |
| `POST /v1/agents/enroll` | 201 credentials | 8 KiB | 5s |
| `POST /v1/agents/renew` | 200 credentials | 8 KiB | 5s |
| `GET /v1/agents/plan` | 200 snapshot / 304 unchanged | 1 MiB | 5s |
| `POST /v1/agents/plan/ack` | 204 | no body consumed | 5s |
| `GET /v1/agents/events` | 200 hints / 204 none | 4 KiB | 30s |
| `GET /v1/agents/work` | 200 command array | 16 KiB, <=16 commands | 5s |
| `POST /v1/agents/work/{id}/claim` | 204 claimed | no body consumed | min(5s, command remainder) |
| `POST /v1/agents/observations` | 200 durable ID ack | 8 KiB | 5s |
| `POST /v1/agents/health` | 204 | no body consumed | 5s |

Connect timeout is 3s. JSON/Protobuf bodies are uncompressed in this draft.
Server request limits should be 8 KiB for enrollment, 1 KiB for renew/claim/plan
ack, 4 KiB for health, and 256 KiB for observation uploads. Servers must enforce
limits before buffering and validate all bodies. Client response limits are
independently enforced while reading. Native upload has at most 64 records;
individual locally retained records are subject to `max_result_bytes`.

Standard failures: 400 malformed/schema, 401 missing/expired/invalid proof, 403
scope/role denied, 404 unknown operation/work, 409 revision/claim conflict, 410
expired work, 413 oversized, 422 invalid declarative content, 429 saturation/quota,
500/503 transient server failure. No data is acknowledged on these failures.
Proposed server error envelope (not currently decoded by the agent):

```json
{"schema_version":1,"error":{"code":"invalid_plan","retryable":false}}
```

Do not include tokens, private targets or raw request bodies in errors. The client
reports status/classes, ignores error text, and uses bounded exponential backoff
with jitter (1s growing to 30s plus <=20%). It currently does not honor Retry-After
or distinguish permanent remote errors; operators must correct persistent errors.
One bad plan does not replace accepted state or prevent observation upload attempts.

## Enrollment, renewal and recovery

Enrollment requires an explicit locally supplied one-time token:

```json
{
  "schema_version":1,
  "token":"ONE_TIME_PROOF",
  "capabilities":["http","plan-v1","long-poll","protobuf-observations-v1"]
}
```

Successful response:

```json
{
  "schema_version":1,
  "agent_id":"edge-03",
  "token":"SCOPED_AGENT_PROOF",
  "expires_unix_ms":1793991600000,
  "deployment":"public_edge"
}
```

Servers bind the one-time token to identity and trusted provisioning policy,
consume it atomically and never permit arbitrary role escalation. Responses must
not be cached by shared intermediaries. The agent persists credentials in its
owner-only local database, prints only identity/success, and never logs the token.
There is no automatic enrollment on installation or standalone startup.

Renew uses current bearer proof and `{"schema_version":1}`, returning the same
credential shape and unchanged agent ID/deployment. The client renews within 60s
of expiry and persists the replacement before using it. For lost replies, the
server must retain an idempotent renewal result keyed by the old credential and
accept that proof **for renewal only**, for a bounded grace period (proposed 5min,
never beyond the old credential's expiry). Repeating renewal returns the same
replacement; it must not cause an endless rotation race. The test server simulates
this lost-response case. The client cannot renew an already expired credential.

Expired or lost identities require explicit recovery enrollment through the
control plane. Re-enrollment into an existing state directory must return the
same issuer and agent ID; otherwise the agent refuses replacement. Retained
observations and revision history survive recovery. Enrollment response loss is
not automatically retried with a consumed one-time token: issue a recovery token
for the same identity. Exact production token lifetimes, revocation and safe
recovery UX remain to be finalized.

## Full desired-state plans

An agent receives only its assignments, not the fleet-wide monitor catalog:

```json
{
  "schema_version":1,
  "revision":42,
  "schedule_epoch_unix_ms":1793894400000,
  "valid_until_unix_ms":1793980800000,
  "recipes":[{
    "id":"http-standard",
    "kind":"http",
    "interval_ms":300000,
    "timeout_ms":5000,
    "http":{
      "method":"GET",
      "expected_status":{"min":200,"max":299},
      "max_redirects":0,
      "inspect_body":false
    }
  }],
  "assignments":[{
    "id":"assignment-001",
    "monitor_id":"monitor-101",
    "recipe_id":"http-standard",
    "target":"https://example.com",
    "phase_offset_ms":100000,
    "jitter_ms":0,
    "workload":"routine"
  }]
}
```

Recipes are reusable within the snapshot, <=256, with unique IDs. Assignments have
unique immutable IDs, refer to an existing recipe, and cannot exceed local
`max_checks` (hard validation bound 4096). HTTP GET/HEAD is implemented; other
kinds are rejected at activation. `http`, jitter and workload have documented
Rust defaults; examples spell them out. Workload is `routine` or `community`.
Community-provisioned agents accept only community assignments. Local opt-in is
still required; a remote plan cannot enable participation.

Revisions increase per authenticated agent. Identical repeated snapshots are
idempotent. Same revision with different content, rollback, invalid schema, unsafe
limits, unknown recipes or expired lease is rejected atomically. A continuing
assignment ID must retain monitor and target identity; new target identity needs
a new assignment ID. Servers must never recycle IDs, including after removal.
Changing recipes, scheduling or lease requires a higher revision.

The lease is absolute. At receipt it must be positive, within the local maximum
(default 24h, configurable up to 7d), and after the schedule epoch. Full validation
and durable last-known-good replacement precede activation. New plans cancel old
work; existing buffered observations retain their original revision. An applied
ack body is `{"schema_version":1,"revision":42}` and may be retried safely.
It is sent only after successful persistence and activation, never for expiry or
rejection. There is no separate negative-ack route in this slice; bounded health
and diagnostics report rejected updates. A production rejection feedback contract
remains open.

`GET /plan` uses `If-None-Match: "42"`. A 200 snapshot requires `ETag: "42"`
matching the body revision. A 304 supplies no replacement document and **does not
extend authorization**. Renewal requires a higher full revision with a new expiry.
No header, heartbeat, event or credential refresh extends the plan lease. Agents
continue authorized work during temporary outages and stop when the lease expires.
There are no incremental patches or externally downloaded manifests.

Schedules use monotonic timers anchored to the epoch, with per-assignment phase
and stable jitter <=5% of interval. Missed cycles are counted and skipped, never
queued for catch-up. Control planes should spread monitor base phases as well as
geographic offsets. The managed product's three locations per 300s is a Cloud
assignment policy; it is absent from universal agent validation.

## Notifications and durable on-demand work

`GET /events?cursor=<opaque>&wait_seconds=25` waits up to 25 seconds. A 204 means
no event; a 200 body is:

```json
{"cursor":"17","plan_changed":true,"work_available":true}
```

Cursors are <=128 bytes. Events are coalescible hints, never durable commands or
authorization. A lost cursor/connection is recovered through ordinary plan/work
reconciliation. Immediate successful polls wait at least 250ms; failures back off.
Long-poll is enabled by default for managed execution and can be disabled locally.
There is one request per agent, not one per assignment.

`GET /work` returns up to 16 durable commands:

```json
[{
  "id":"command-001",
  "run_id":"live-run-001",
  "monitor_id":"monitor-101",
  "assignment_id":"assignment-001",
  "revision":42,
  "deadline_unix_ms":1793894440000,
  "priority":"urgent",
  "cancelled":false
}]
```

The current slice references an assigned recipe/target rather than accepting an
arbitrary new probe definition. Revision, assignment and monitor must match the
active plan. Deadlines must be in the next 60 seconds and cannot extend the plan
lease. Priority is `normal` or `urgent` (default). Community work always uses its
own stricter class even when urgent. Commercial per-run/daily/customer limits
belong to the control plane, not the agent.

Before claim, local admission must succeed and a command/observation-ID reservation
must be durably stored. `POST /work/{id}/claim` sends `{"schema_version":1}`.
The server atomically validates ownership, assignment, deadline, cancellation and
claim state. Return 204 for an accepted claim (idempotent for the same agent),
409 for unavailable/cancelled/conflicting work, 410 for expiry. No probe starts
without an explicit 204. Local saturation does not claim work: it may be offered
again until deadline. There is no unbounded pending-command queue in the agent.

A lost claim response is uncertain and produces a SKIPPED observation; the client
does not execute or re-claim that reserved ID. A crash after reservation yields
ERROR `execution_interrupted_outcome_unknown` on restart with its reserved result
ID. This deliberately avoids duplicate command execution. Servers should time out
claimed work without a result and allow a new command ID for a fresh user request.
There is no exactly-once network measurement guarantee.

To cancel active work, retain the command with `cancelled:true` and notify agents.
The agent cancels only a matching command/run pair; unknown cancellation tombstones
are ignored locally. The server must keep terminal cancellation/claim state until
at least deadline plus its retry window, never reissue a cancelled ID, and reject
late claims. Client deduplication retains <=1024 reservations through deadline,
then expires them; expired replay is independently rejected. Plan replacement,
lease expiry, shutdown and local community opt-out also cancel affected work.

Live results carry `origin=LIVE`, command_id and run_id; they must not silently
enter routine uptime denominators. Community Live results still carry command/run
correlation, while admission remains community-limited. The server owns run
completion, location aggregation and incident decisions.

## Observation delivery and exact durable acknowledgements

`POST /observations` sends `Content-Type: application/x-protobuf` with a v1 Batch.
The [public schema](../proto/pulse/v1/observations.proto) matches manually derived
Rust prost messages; no protoc/gRPC server is required. At most 64 observations
and 256 KiB are sent per batch, including framing. A result ID is a UUID created
once and reused through persistence, restart and retry. The server derives agent
identity from authentication; the observation cannot select an agent or tenant.

Outcomes: PASS policy satisfied; FAIL response/policy violation; ERROR execution
or transport failure; SKIPPED local refusal/cancellation. Unspecified/unknown enum
values never count as success. Origins distinguish ROUTINE, LIVE, COMMUNITY and
MANUAL. Native timestamps are UTC milliseconds, total and optional phase durations
are microseconds. Missing DNS/TCP/TLS/TTFB measurements remain absent, never zero.
The current HTTP engine does not supply those precise phases. Response content,
request secrets and raw URLs are not included in observations.

Success requires both HTTP 200 and a validated JSON body:

```json
{"schema_version":1,"accepted_ids":["778ca338-53c4-4b6b-8eb9-96ec42ab81ad"]}
```

Every returned ID must be unique and present in that exact upload. Partial acks
are valid. An empty ack retains everything. IDs mean the server has **durably
committed** those records and their deduplication keys, surviving a server restart.
Returning success after merely enqueueing in volatile memory violates the contract.
The client removes only those exact records in a local transaction.

The server must deduplicate `(authenticated_agent_id, observation_id)` and accept
an identical retry without creating another measurement. Different content under
the same key is a conflict. A lost HTTP reply or malformed/unknown-ID ack causes
safe retry; HTTP 202/204 alone is not acceptance. Deduplication retention must cover
the entire retry/retention horizon (including operator recovery), preferably for
the lifetime of stored observation IDs. Stable record IDs make a batch-level
idempotency key unnecessary. Server ACK loss is covered by the local HTTPS tests.

Local retention is bounded, with newest-drop overflow and explicit dropped counts.
At-least-once delivery applies to durably retained observations while the filesystem
remains healthy; it does not promise no loss before persistence or on overflow.
Permanent per-record rejection/dead-letter behavior and production retention TTLs
remain open. A poison batch currently remains queued and requires remediation.

## Health and capability evolution

`POST /health` sends the current Health object: phase, revision, active/peak probes,
completed, skipped, missed, dropped and plan_expired. These are bounded counters,
not private CPU/memory/process metrics. Status snapshots are eventually updated
and not an authorization mechanism. Health is best-effort and returns 204.

Capabilities are advertised during enrollment. The present client always supports
the documented HTTP/plan/long-poll/Protobuf set; selecting an incompatible future
schema must fail explicitly. Before declaring this protocol stable, finalize
capability negotiation, credential scoping, error/rejection feedback, replay
retention, database migrations and clock assumptions with independent control-plane
implementers. Proprietary billing, frontend, alerting and incident logic are outside
this public contract.
