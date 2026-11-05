# Local resource measurements

These short measurements check bounded behavior, not production capacity. Run:

```sh
just measure
# Or use an existing release build:
python3 scripts/measure-agent.py --binary target/release/pulse --assignments 1000 --seconds 8
```

The Python standard-library fixture binds loopback, authorizes private probing in
its temporary configuration, and never contacts external targets or a control
plane. It samples only the Pulse process through `/proc` every 50ms. It terminates
the child with SIGTERM and reads its final local health. Temporary state/output is
removed. Python 3 and Linux `/proc` are required.

## Recorded run

2026-11-05, Linux 6.12.113 x86_64, AMD Ryzen 9 8945HX, Rust 1.99.1, optimized
release profile, default Cargo features. Each scenario ran for approximately 8s
with 1,000 assignments, concurrency 8 (one slot reserved for urgent work), 16
connection slots, a 32 MiB probe-memory reservation and 50 starts/s. Normal work
uses a 300s interval; overload and timeout work request a 1s interval. All phases
are spread evenly. Responses are two bytes; inspection is disabled. The timeout
fixture delays every response by 2s while the agent deadline is 500ms.

| Scenario | CPU % of one core | Peak sampled RSS MiB | Threads | FDs / sockets | p95 schedule delay ms | Completed / skipped admission | SIGTERM exit ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Normal, 3.33 requested/s | 0.12 | 9.18 | 2 | 12 / 3 | 2 | 27 / 0 | 1.2 |
| Overload, 1000 requested/s | 1.37 | 9.42 | 2 | 13 / 4 | 1 | 390 / 7645 | 0.7 |
| Every running request times out | 0.75 | 9.23 | 2 | 19 / 10 | 1 | 113 / 7925 | 1.0 |

The timeout run produced 106 timeout ERROR observations and seven SKIPPED
cancellations on shutdown. Its total skipped counter is 7,932, including those
seven cancellations. Peak active probes were seven, preserving urgent capacity.
Every scenario ended with zero active probes and zero dropped output. Missed-cycle
counters were zero; most excess due checks were refused at admission rather than
left waiting. These results show bounded refusal under the tested load, not that
1,000 checks/s were executed.

[Raw measurements](measurements/standalone-2026-11-05.jsonl) include exact counters.
CPU uses process user/system ticks divided by elapsed wall time, so short runs have
coarse CPU resolution. RSS/FD/thread peaks are sampled and may miss brief peaks;
socket counts include runtime descriptors. Schedule delay is a UTC millisecond
approximation derived from configured phases, separate from probe latency.

This run uses plain loopback HTTP and stdout, not TLS, managed SQLite result
commits, upload retry or diagnostic export. Allocations and Tokio task counts were
not profiled; bounded owned task counts are checked through execution health and
the governor tests. Integration tests exercise the managed HTTPS and persistence
boundaries separately. Production approval requires sustained TLS/DNS/managed and
failure testing on the actual edge hardware/filesystem, with OS resource limits
and the routing workload present. See [rollout decisions](ARCHITECTURE.md#verification-and-remaining-rollout-decisions).
