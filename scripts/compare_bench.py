#!/usr/bin/env python3
"""Extract labeled wall-clock metrics from a `cargo bench` tee file and compare
them against a historical baseline. Regressions use three tiers:

    10–50%  blue notice
    50–85%  yellow warning
    85%+    red failure (fails the job)

The benches in `benches/` use a custom harness (`std::time::Instant`) rather
than criterion, and print labeled millisecond metrics in a couple of shapes:

    label: <value> ms
    label: <value> ms/op

This script extracts those into `{label: milliseconds}`, compares each current
value against a historical baseline (`--best`, one or more JSON snapshots),
FAILS if any metric regressed by at least `--failure-margin` (default 85%),
warns above `--warning-margin` (default 50%), notices above `--notice-margin`
(default 10%), and writes the all-time best (the fastest value seen for each
label) to `--out`.

The regression baseline is the median of the most recent successful raw runs
(`--history`, up to five values per label). When history is not available yet,
it falls back to the median of the supplied published-best snapshots. This
keeps one lucky run on a faster host from setting an unreachable baseline while
still retaining the all-time best for the published record.

Regressions whose absolute delta is below `--min-absolute-delta` (default
0.05ms) are ignored entirely. Shared-CI jitter on single-digit-microsecond
microbenches can exceed 85% relative while being a few microseconds of absolute
noise; the floor keeps those from permanently failing the job while still
ratcheting a new best when one is set.

When the runner prints benchmark section headers, they are included in each
metric key so equal operation names from separate benchmarks do not collide.

Exit code is 1 if any metric regressed by at least the failure margin, else 0. This is
intended to be driven from `.github/workflows/benches.yml` after teeing the
bench output to a file.

NOTE: wall-clock benchmarks on shared CI runners are noisy. Blue notices
surface small drift, yellow warnings flag moderate regressions, and the 85%
red threshold catches severe regressions only.
"""

from __future__ import annotations

import argparse
import json
import re
import statistics
import sys

HISTORY_LEN = 5

# `label: <number> ms` or `label: <number> ms/op` or `label: <number> ns/op`.
# Deliberately does NOT match the ratio lines ("=> foo is 1.23x faster than
# bar") or the parenthesized `label: (setup: ..., algo: ..., ...)` forms,
# so only the stable per-op/total metrics are tracked.
METRIC = re.compile(
    r"^(.+?):\s+([0-9]+(?:\.[0-9]+)?)\s*(ns|ms)(?:/op)?(?:\s*\([^)]*\))?\s*$"
)
CHECKPOINT = re.compile(r"^\s*S=(\d+):\s*$")
BENCHMARK_SECTION = re.compile(r"^\s*\[([^]]+)\]\s+BENCHMARK:\s*(.+?)\s*$")


def extract(path: str) -> dict[str, float]:
    """Parse the tee'd bench output into {label: milliseconds}."""
    metrics: dict[str, float] = {}
    checkpoint: str | None = None
    benchmark: str | None = None
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            section_match = BENCHMARK_SECTION.match(line)
            if section_match:
                benchmark = f"{section_match.group(1)}/{section_match.group(2)}"
                checkpoint = None
                continue
            checkpoint_match = CHECKPOINT.match(line)
            if checkpoint_match:
                checkpoint = checkpoint_match.group(1)
                continue
            m = METRIC.match(line)
            if not m:
                continue
            label = m.group(1).strip()
            if benchmark is not None:
                label = f"{benchmark}: {label}"
            if checkpoint is not None:
                label = f"S={checkpoint}: {label}"
            if label in metrics:
                raise ValueError(f"duplicate benchmark label: {label}")
            value = float(m.group(2))
            unit = m.group(3)
            # Normalise nanoseconds to milliseconds for consistent comparison.
            if unit == "ns":
                value /= 1_000_000.0
            metrics[label] = value
    return metrics


def parse_args() -> argparse.Namespace:
    """Parse the benchmark comparison command line."""
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--current", required=True, help="tee'd bench output")
    ap.add_argument(
        "--best",
        required=True,
        nargs="+",
        metavar="JSON",
        help="one or more previous benchmark JSON snapshots (bench.json)",
    )
    ap.add_argument("--out", required=True, help="path to write the running best JSON")
    ap.add_argument(
        "--history",
        help="JSON of recent raw-run metrics (optional; bootstraps from --best)",
    )
    ap.add_argument(
        "--history-out",
        help="path to write recent raw-run metrics (optional)",
    )
    ap.add_argument(
        "--notice-margin",
        type=float,
        default=0.10,
        help="blue notice above this regression fraction (default 10%%)",
    )
    ap.add_argument(
        "--warning-margin",
        "--margin",
        dest="warning_margin",
        type=float,
        default=0.50,
        help="yellow warn above this regression fraction (default 50%%)",
    )
    ap.add_argument(
        "--failure-margin",
        type=float,
        default=0.85,
        help="fail at or above this regression fraction (default 85%%)",
    )
    ap.add_argument(
        "--min-absolute-delta",
        type=float,
        default=0.05,
        help="ignore regressions whose absolute delta is below this many "
        "milliseconds (default 0.05ms = 50us)",
    )
    args = ap.parse_args()
    if not 0.0 < args.notice_margin < args.warning_margin < args.failure_margin:
        ap.error(
            "margins must satisfy 0 < notice-margin < warning-margin < failure-margin"
        )
    if args.min_absolute_delta < 0.0:
        ap.error("min-absolute-delta must be >= 0")
    return args


def load_best(paths: list[str]) -> dict[str, list[float]]:
    """Load prior benchmark snapshots, treating missing or invalid JSON as empty."""
    history: dict[str, list[float]] = {}
    for path in paths:
        try:
            with open(path, encoding="utf-8") as fh:
                snapshot = json.load(fh)
        except (FileNotFoundError, json.JSONDecodeError):
            continue
        if not isinstance(snapshot, dict):
            continue
        for label, value in snapshot.items():
            if isinstance(value, (int, float)):
                history.setdefault(label, []).append(float(value))
    return history


def load_history(path: str | None) -> dict[str, list[float]]:
    """Load recent raw-run values, keeping only the newest samples per label."""
    if path is None:
        return {}
    try:
        with open(path, encoding="utf-8") as fh:
            snapshot = json.load(fh)
    except (FileNotFoundError, json.JSONDecodeError):
        return {}
    if not isinstance(snapshot, dict):
        return {}
    history: dict[str, list[float]] = {}
    for label, values in snapshot.items():
        if isinstance(values, (int, float)):
            samples = [float(values)]
        elif isinstance(values, list):
            samples = [
                float(value) for value in values if isinstance(value, (int, float))
            ]
        else:
            continue
        if samples:
            history[label] = samples[-HISTORY_LEN:]
    return history


def legacy_label_counts(current: dict[str, float]) -> dict[str, int]:
    """Count suffixes for safe migration of pre-section metric labels."""
    counts: dict[str, int] = {}
    for label in current:
        suffix = label.rsplit(": ", 1)[-1]
        counts[suffix] = counts.get(suffix, 0) + 1
    return counts


def previous_values(
    label: str, history: dict[str, list[float]], suffix_counts: dict[str, int]
) -> list[float]:
    """Find historical values, migrating a bare legacy label only when unambiguous."""
    if label in history:
        return history[label]
    suffix = label.rsplit(": ", 1)[-1]
    return history.get(suffix, []) if suffix_counts.get(suffix) == 1 else []


def compare_metrics(
    current: dict[str, float],
    snapshots: dict[str, list[float]],
    history: dict[str, list[float]],
    notice_margin: float,
    warning_margin: float,
    failure_margin: float,
    min_absolute_delta: float,
) -> tuple[
    dict[str, float],
    dict[str, list[float]],
    dict[str, float],
    list[str],
    list[str],
    list[str],
]:
    """Update best/history values and classify regressions."""
    suffix_counts = legacy_label_counts(current)
    new_best = {label: min(values) for label, values in snapshots.items() if values}
    for label, values in history.items():
        if values:
            new_best[label] = min(new_best.get(label, values[0]), *values)
    # Only keep metrics present in this run. A missing label breaks the run
    # sequence, so values from before that gap must not become its baseline if
    # the metric later reappears.
    new_history: dict[str, list[float]] = {}
    baselines: dict[str, float] = {}
    notices: list[str] = []
    warnings: list[str] = []
    failures: list[str] = []
    for label, value in sorted(current.items()):
        history_values = previous_values(label, history, suffix_counts)
        snapshot_values = previous_values(label, snapshots, suffix_counts)
        if history_values:
            # Median of recent raw runs tolerates a slow or fast host without
            # ratcheting to the all-time luckiest run.
            prior = statistics.median(history_values)
        elif snapshot_values:
            # Bootstrap: without raw history, use the median published best.
            prior = statistics.median(snapshot_values)
        else:
            prior = None

        all_values = [value, *history_values, *snapshot_values]
        new_best[label] = min(all_values)

        if prior is None:
            new_history[label] = [value]
            continue
        baselines[label] = prior
        if value < prior:
            new_history[label] = [*history_values, value][-HISTORY_LEN:]
            continue
        delta = value - prior
        # Sub-floor deltas are pure runner jitter on microbenches: skip
        # classification entirely (the new-best ratchet above already ran).
        if delta < min_absolute_delta:
            new_history[label] = [*history_values, value][-HISTORY_LEN:]
            continue
        increase = (
            value / prior - 1.0 if prior > 0 else (float("inf") if value > 0 else 0.0)
        )
        message = (
            f"{label}: {value:.4f}ms is {increase * 100:.1f}% above baseline "
            f"{prior:.4f}ms (+{delta:.4f}ms)"
        )
        bootstrap_failure = increase >= failure_margin and len(history_values) < 3
        if increase >= failure_margin and not bootstrap_failure:
            failures.append(message)
        elif increase >= failure_margin:
            warnings.append(
                f"{message} (baseline bootstrap: {len(history_values)}/3 raw runs)"
            )
        elif increase >= warning_margin:
            warnings.append(message)
        elif increase > notice_margin:
            notices.append(message)

        # Do not let a failure-sized bootstrap outlier become the raw-run
        # baseline. It remains a warning until comparable runs establish a
        # baseline, rather than normalizing a persistent slowdown.
        if not bootstrap_failure:
            samples = [*history_values, value]
            new_history[label] = samples[-HISTORY_LEN:]
    return new_best, new_history, baselines, notices, warnings, failures


def write_best(path: str, metrics: dict[str, float]) -> None:
    """Write the updated best metrics as formatted JSON."""
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(metrics, fh, indent=2, sort_keys=True)
        fh.write("\n")


def write_history(path: str, history: dict[str, list[float]]) -> None:
    """Write recent raw-run metrics as formatted JSON."""
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(history, fh, indent=2, sort_keys=True)
        fh.write("\n")


def report_results(
    current: dict[str, float],
    baselines: dict[str, float],
    notices: list[str],
    warnings: list[str],
    failures: list[str],
) -> None:
    """Print the metrics and GitHub notice/warning/error annotations."""
    print(
        f"parsed {len(current)} metrics, comparing against {len(baselines)} historical baselines"
    )
    for label, value in sorted(current.items()):
        prior = baselines.get(label)
        suffix = "" if prior is None else f" (baseline {prior:.4f}ms)"
        print(f"  {label}: {value:.4f}ms{suffix}")
    if notices:
        print("\nBLUE NOTICES (10–50%):")
        for notice in notices:
            print(f"  NOTICE {notice}")
            print(f"::notice title=Benchmark regression::{notice}")
    if warnings:
        print("\nYELLOW WARNINGS (50–85%, not failing):")
        for warning in warnings:
            print(f"  WARN {warning}")
            print(f"::warning title=Benchmark regression::{warning}")
    if failures:
        print("\nRED REGRESSIONS (85%+, failing):")
        for failure in failures:
            print(f"  FAIL {failure}")
            print(f"::error title=Benchmark regression::{failure}")


def main() -> int:
    """Compare current benchmarks with the baseline and report regressions."""
    args = parse_args()
    try:
        current = extract(args.current)
    except (OSError, ValueError) as exc:
        print(f"could not parse benchmark output: {exc}", file=sys.stderr)
        return 1
    if not current:
        print("no benchmark metrics parsed", file=sys.stderr)
        return 1
    snapshots = load_best(args.best)
    history = load_history(args.history)
    new_best, new_history, baselines, notices, warnings, failures = compare_metrics(
        current,
        snapshots,
        history,
        args.notice_margin,
        args.warning_margin,
        args.failure_margin,
        args.min_absolute_delta,
    )
    write_best(args.out, new_best)
    if args.history_out:
        write_history(args.history_out, new_history)
    report_results(current, baselines, notices, warnings, failures)
    if failures:
        return 1

    print("\nOK: no red regressions")
    return 0


if __name__ == "__main__":
    sys.exit(main())
