#!/usr/bin/env python3
"""Run repeatable local streaming benchmarks and save the commands, JSON, and HTML report."""

import argparse
import datetime as dt
import hashlib
import html
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
SCENARIOS = {
    "healthy": {"upload-mibps": "8", "rtt-ms": "40", "watch": "1"},
    "constrained": {"upload-mibps": "2", "rtt-ms": "40", "watch": "1"},
    "high-latency": {"upload-mibps": "2", "rtt-ms": "150", "watch": "1"},
    "peer-dropout": {
        "upload-mibps": "2", "rtt-ms": "60", "watch": "3",
        "peers": "up=2,rtt=60,offline=3/8;up=0.5,rtt=120",
    },
    "4k-rate": {"upload-mibps": "16", "rtt-ms": "40", "watch": "1",
                "size-mib": "2048", "duration": "341.333333", "startup-mib": "8", "seek-mib": "8"},
    "large-file": {"upload-mibps": "16", "rtt-ms": "40", "watch": "1",
                   "size-mib": "8192", "duration": "1365.333333", "startup-mib": "8", "seek-mib": "8"},
}


def make_report(manifest, output):
    rows = []
    for case in manifest["cases"]:
        if case["status"] != "passed":
            rows.append(f'<tr><td>{html.escape(case["scenario"])}</td><td>{case["engine"]}</td>'
                        '<td colspan="7">Failed; see case log.</td></tr>')
            continue
        report = json.loads((output / case["report"]).read_text())
        runs = report["runs"]
        startup, seek = report["startup"], report["seek"]
        watched = sum(r["watch_ms"] for r in runs)
        stalled = sum(r["stall_ms"] for r in runs)
        case["summary"] = {
            "startup_median_ms": startup["median_ms"], "startup_p95_ms": startup["p95_ms"],
            "seek_median_ms": seek["median_ms"], "seek_p95_ms": seek["p95_ms"],
            "stalls": sum(r["stalls"] for r in runs),
            "stall_fraction": stalled / watched if watched else 0,
            "median_peers_live": statistics.median(r["peers_live"] for r in runs),
            "median_throughput_mibps": statistics.median(r["avg_download_bps"] for r in runs) / 1048576,
        }
        s = case["summary"]
        config = case.get("configuration", {"size-mib": "256", "duration": "256"})
        size_mib = float(config["size-mib"])
        media_mibps = size_mib / float(config["duration"])
        rows.append(f'<tr><td>{html.escape(case["scenario"])}</td><td>{case["engine"]}</td>'
                    f'<td>{size_mib / 1024:g} GiB / {media_mibps:g} MiB/s</td>'
                    f'<td>{s["startup_median_ms"]:.0f} / {s["startup_p95_ms"]:.0f}</td>'
                    f'<td>{s["seek_median_ms"]:.0f} / {s["seek_p95_ms"]:.0f}</td>'
                    f'<td>{s["stalls"]}</td><td>{100*s["stall_fraction"]:.1f}%</td>'
                    f'<td>{s["median_peers_live"]:g}</td><td>{s["median_throughput_mibps"]:.2f}</td></tr>')
    title = html.escape(manifest["label"])
    document = f'''<!doctype html><html lang="en"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title>
<style>body{{font:16px system-ui;margin:2rem auto;padding:0 1rem;max-width:1150px;background:#11151d;color:#e8edf5}}
table{{border-collapse:collapse;width:100%}}th,td{{padding:.8rem;text-align:left;border-bottom:1px solid #344154}}
th{{color:#a5cbff}}p{{line-height:1.6;color:#b9c7db}}.scroll{{overflow:auto}}code{{color:#a5cbff}}</style>
<h1>{title}</h1><p>{manifest["started_utc"]} · {manifest["runs_per_case"]} cold-cache runs per case ·
Synthetic video data · seek targets 10%, 50%, 90%, 25%.</p>
<p>These measure when bytes are ready through the real streaming server. Decoder time to first frame
is separate. All traffic stays on loopback; the selected VPN is not measured by this matrix.
Startup has only {manifest["runs_per_case"]} samples per case, so p95 is descriptive, not a stable tail estimate.</p>
<div class="scroll"><table><thead><tr><th>Scenario</th><th>Engine</th><th>File / media rate</th><th>Startup ms<br>median / p95</th>
<th>Seek ms<br>median / p95</th><th>Stalls</th><th>Time stalled</th><th>Live peers<br>median</th>
<th>MiB/s<br>median</th></tr></thead><tbody>{''.join(rows)}</tbody></table></div>
<p>Healthy: 8 MiB/s, 40 ms RTT. Constrained: 2 MiB/s, 40 ms. High latency: 2 MiB/s, 150 ms.
Peer dropout: one 2 MiB/s peer disappears during seconds 3–8; a 0.5 MiB/s peer remains.
The dropout temporarily leaves less bandwidth than playback needs.</p>
<p>4K-rate: 2 GiB at 6 MiB/s media rate (about 50 Mb/s), on a 16 MiB/s link.
Large-file: 8 GiB at the same rate. Both request 8 MiB from each end at startup and 8 MiB
at each seek. These are byte-level workloads; they do not decode 4K frames.</p>
<p>Full commands, source and binary hashes, platform information, and tuning overrides are in
<a href="manifest.json">manifest.json</a>. Individual case JSON files contain every measurement.</p></html>'''
    (output / "report.html").write_text(document)
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/torrentplayer-bench")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--scenario", choices=SCENARIOS, action="append")
    parser.add_argument("--engine", choices=["rqbit", "swarm"], action="append")
    parser.add_argument("--queue-ms", type=int, help="Override the swarm engine request queue only")
    parser.add_argument("--label", default="TorrentPlayer streaming benchmarks")
    args = parser.parse_args()
    if args.runs < 1 or (args.queue_ms is not None and args.queue_ms < 1):
        parser.error("runs and queue-ms must be positive")
    binary = args.binary.resolve(strict=True)
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = (args.out or ROOT / "target/streaming-benchmarks" / stamp).resolve()
    output.mkdir(parents=True, exist_ok=False)
    source_files = ["Cargo.lock", "crates/torrentplayer-swarm/src/sched.rs", "crates/torrentplayer-bench/src/lib.rs", "crates/torrentplayer-bench/src/seeder.rs",
                    "crates/torrentplayer-bench/src/http.rs", "crates/torrentplayer-torrent/src/backend.rs"]
    manifest = {
        "label": args.label, "started_utc": stamp, "runs_per_case": args.runs,
        "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "benchmark_platform": "Windows" if binary.suffix.lower() == ".exe" else platform.system(),
        "runner_architecture": platform.machine(),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "source_sha256": {p: hashlib.sha256((ROOT/p).read_bytes()).hexdigest() for p in source_files},
        "queue_ms_override": args.queue_ms, "cases": [],
    }
    failed = False
    for scenario in args.scenario or SCENARIOS:
        for engine in args.engine or ["rqbit", "swarm"]:
            name = f"{scenario}-{engine}"
            config = {"size-mib": "256", "duration": "256", **SCENARIOS[scenario]}
            report_path = str(output / f"{name}.json")
            if binary.suffix == ".exe" and os.name != "nt":
                report_path = subprocess.check_output(["wslpath", "-w", report_path], text=True).strip()
            command = [str(binary), "local",
                       "--runs", str(args.runs), "--engine", engine,
                       "--json", report_path]
            for flag, value in config.items():
                command.extend([f"--{flag}", value])
            if args.queue_ms is not None and engine == "swarm":
                command.extend(["--queue-ms", str(args.queue_ms)])
            print(f"Running {name} ({args.runs} cold-cache runs)", flush=True)
            started = time.monotonic()
            case = {"scenario": scenario, "engine": engine, "command": command,
                    "configuration": config,
                    "report": f"{name}.json", "log": f"{name}.log", "status": "running"}
            manifest["cases"].append(case)
            with (output / case["log"]).open("w") as log:
                try:
                    result = subprocess.run(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT,
                                            timeout=max(180, args.runs*90), check=False,
                                            env={**os.environ, "RUST_LOG": "error"})
                    case["status"] = "passed" if result.returncode == 0 else "failed"
                    case["exit_code"] = result.returncode
                except subprocess.TimeoutExpired:
                    case["status"] = "timed-out"
            case["elapsed_seconds"] = time.monotonic()-started
            failed |= case["status"] != "passed"
            make_report(manifest, output)
            print(json.dumps({"case": name, "status": case["status"], **case.get("summary", {})}), flush=True)
    print(f"Report: {output/'report.html'}", flush=True)
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
