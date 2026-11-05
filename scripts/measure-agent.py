#!/usr/bin/env python3
"""Measure one Linux Pulse process against a local HTTP fixture; no external traffic."""
import argparse
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time


class Target(http.server.BaseHTTPRequestHandler):
    delay = 0

    def do_GET(self):
        time.sleep(self.delay)
        try:
            self.send_response(200)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *_args):
        pass


def sample(pid):
    root = Path(f"/proc/{pid}")
    stat = (root / "stat").read_text().split()
    links = []
    for path in (root / "fd").iterdir():
        try:
            links.append(os.readlink(path))
        except FileNotFoundError:
            pass
    rss = int(stat[23]) * os.sysconf("SC_PAGE_SIZE")
    return {
        "cpu_ticks": int(stat[13]) + int(stat[14]),
        "rss": rss,
        "threads": len(list((root / "task").iterdir())),
        "fds": len(links),
        "sockets": sum(link.startswith("socket:") for link in links),
    }


def measure(binary, assignments, seconds, scenario):
    interval = 300000 if scenario == "normal" else 1000
    Target.delay = 2 if scenario == "timeouts" else 0
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Target)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="pulse-measure-") as directory:
            root = Path(directory)
            text = f'''[agent]
mode="standalone"
state_dir="{root / 'state'}"
[network]
allow_private_networks=true
[output]
stdout=true
[limits]
max_checks=4096
max_concurrent_probes=8
max_connections=16
max_probe_memory_bytes=33554432
max_probe_starts_per_second=50
'''
            for index in range(assignments):
                text += f'''[[probes]]
id="probe-{index}"
kind="http"
target="http://127.0.0.1:{server.server_port}/"
interval_ms={interval}
timeout_ms=500
phase_offset_ms={index * interval // assignments}
'''
            config = root / "pulse.toml"
            config.write_text(text)
            env = {key: value for key, value in os.environ.items() if not key.startswith("OTEL_")}
            env["RUST_LOG"] = "off"
            env["NO_COLOR"] = "1"
            records_path = root / "observations.jsonl"
            with records_path.open("w") as records, (root / "stderr").open("w") as errors:
                process = subprocess.Popen([binary, "run", "--config", str(config)],
                                           stdout=records, stderr=errors, env=env)
                started = time.monotonic()
                peaks = dict.fromkeys(["rss", "threads", "fds", "sockets"], 0)
                last = {"cpu_ticks": 0}
                try:
                    while time.monotonic() - started < seconds:
                        if process.poll() is not None:
                            raise RuntimeError((root / "stderr").read_text())
                        last = sample(process.pid)
                        for key in peaks:
                            peaks[key] = max(peaks[key], last[key])
                        time.sleep(0.05)
                    elapsed = time.monotonic() - started
                    stopped = time.monotonic()
                    process.send_signal(signal.SIGTERM)
                    process.wait(timeout=5)
                    shutdown_ms = round((time.monotonic() - stopped) * 1000, 1)
                    if process.returncode:
                        raise RuntimeError((root / "stderr").read_text())
                finally:
                    if process.poll() is None:
                        process.kill()
                        process.wait()
            observations = [json.loads(line) for line in records_path.read_text().splitlines()]
            lateness = []
            outcomes = {}
            for record in observations:
                index = int(record["assignment_id"].split("-")[-1])
                phase = index * interval // assignments
                lateness.append((record["started_unix_ms"] - phase) % interval)
                outcomes[record["outcome"]] = outcomes.get(record["outcome"], 0) + 1
            lateness.sort()
            status = json.loads(subprocess.check_output(
                [binary, "status", "--state-dir", str(root / "state")], env=env))
            return {
                "scenario": scenario, "assignments": assignments, "interval_ms": interval,
                "seconds": round(elapsed, 2),
                "cpu_percent_one_core": round(last["cpu_ticks"] / os.sysconf("SC_CLK_TCK") / elapsed * 100, 2),
                "peak_sampled_rss_mib": round(peaks["rss"] / 1048576, 2),
                "peak_threads": peaks["threads"], "peak_fds": peaks["fds"],
                "peak_socket_fds": peaks["sockets"], "shutdown_ms": shutdown_ms,
                "observations": len(observations), "outcomes": outcomes,
                "schedule_lateness_p95_ms": lateness[int(len(lateness) * 0.95)] if lateness else None,
                "health": status["health"],
            }
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/pulse")
    parser.add_argument("--assignments", type=int, default=1000)
    parser.add_argument("--seconds", type=int, default=8)
    args = parser.parse_args()
    if not 1 <= args.assignments <= 4096 or args.seconds < 2:
        parser.error("assignments must be 1..4096 and seconds >=2")
    for name in ("normal", "overload", "timeouts"):
        print(json.dumps(measure(str(Path(args.binary).resolve()), args.assignments, args.seconds, name)), flush=True)
