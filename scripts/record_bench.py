#!/usr/bin/env python3
"""Append one successful benchmark run to a durable CSV history."""

from __future__ import annotations

import argparse
import csv
import subprocess
from datetime import datetime, timezone
from pathlib import Path

from compare_bench import extract

FIELDS = (
    "timestamp_utc",
    "revision",
    "branch",
    "metric",
    "milliseconds",
    "work_units",
)


def git_value(*args: str) -> str:
    """Return trimmed output from a Git command."""
    return subprocess.check_output(["git", *args], text=True).strip()


def extract_work_units(path: str) -> dict[str, str]:
    """Parse the ignored GF(2^256) calibration harness output.

    The harness prints CSV rows shaped like
    ``pattern,scenario,scope,outcome,samples,p50,p95,p99,max``. Each
    percentile becomes its own work-unit metric, matching the names already
    used in the history CSV.
    """
    work_units: dict[str, str] = {}
    with open(path, newline="", encoding="utf-8", errors="replace") as stream:
        reader = csv.reader(stream)
        expected = ["pattern", "scenario", "scope", "outcome", "samples"]
        # `cargo test --nocapture` writes status lines before the CSV header.
        for header in reader:
            if header[: len(expected)] == expected:
                break
        else:
            raise SystemExit(
                f"{path}: no calibration header with prefix {expected} found"
            )
        for row in reader:
            if len(row) < 9 or row[0].startswith("#"):
                continue
            pattern, scenario, scope, outcome, samples = row[:5]
            base = (
                f"gf256_work/{pattern}/{scenario}/{scope}/{outcome}/samples={samples}"
            )
            for label, value in zip(("p50", "p95", "p99", "max"), row[5:9]):
                key = f"{base}/{label}"
                if key in work_units:
                    raise SystemExit(f"{path}: duplicate calibration key {key}")
                work_units[key] = value
    return work_units


def main() -> int:
    """Append this successful benchmark run to the CSV history."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--current", help="captured wall-clock benchmark output")
    parser.add_argument(
        "--work-units",
        help="captured GF(2^256) calibration harness output (work units)",
    )
    parser.add_argument(
        "--out", default="benches/bench-results.csv", help="CSV history path"
    )
    args = parser.parse_args()

    metrics = extract(args.current) if args.current else {}
    work_units_in = extract_work_units(args.work_units) if args.work_units else {}
    if not metrics and not work_units_in:
        parser.error("no benchmark metrics parsed; refusing to append an empty run")

    timestamp = datetime.now(timezone.utc).isoformat(timespec="seconds")
    revision = git_value("rev-parse", "--short", "HEAD")
    branch = git_value("branch", "--show-current") or "detached"
    output = Path(args.out)
    needs_header = not output.exists() or output.stat().st_size == 0
    if not needs_header:
        with output.open(newline="", encoding="utf-8") as existing:
            header = next(csv.reader(existing), [])
        if tuple(header) != FIELDS:
            parser.error(
                f"{output} has header {header}, expected {list(FIELDS)}; "
                "migrate it before appending"
            )

    with output.open("a", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=FIELDS, lineterminator="\n")
        if needs_header:
            writer.writeheader()
        for metric, milliseconds in sorted(metrics.items()):
            writer.writerow(
                {
                    "timestamp_utc": timestamp,
                    "revision": revision,
                    "branch": branch,
                    "metric": metric,
                    "milliseconds": f"{milliseconds:.9f}",
                    "work_units": "",
                }
            )
        for metric, work in sorted(work_units_in.items()):
            writer.writerow(
                {
                    "timestamp_utc": timestamp,
                    "revision": revision,
                    "branch": branch,
                    "metric": metric,
                    "milliseconds": "",
                    "work_units": work,
                }
            )

    print(
        f"appended {len(metrics)} benchmark metrics and "
        f"{len(work_units_in)} work-unit metrics to {output}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
