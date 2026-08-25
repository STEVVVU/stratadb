#!/usr/bin/env python
"""Producer for the paired `read/path-decomposition` table in the README.

## Why this script exists

The README publishes the read-path decomposition as a **median over N whole-process
runs** with per-run paired ratios, because the between-run spread on this workload
is wider than criterion's own within-run confidence interval (the `reused-buffer`
variant ranged 10.6-66.0 us across runs). A single
`cargo bench --bench engine -- read/path-decomposition` therefore cannot produce
that table: it yields one run's intervals, not medians-of-N, not paired ratios, and
not win counts.

Before this file existed, that aggregation was done by an uncommitted driver. A
published figure whose producer is committed nowhere is not reproducible, so the
driver is committed here.

## What "paired" means, and why it matters

Ratios are computed **within each run and then aggregated**, never as a quotient of
two aggregates. Machine-state drift (thermal state, page-cache contents, background
load) moves all four variants of a given run together, so a within-run quotient
cancels it. Dividing a median by a median does not, and inflates the range.

## Reproducing

    python scripts/paired_read_decomposition.py --runs 11

Expect this to take several minutes: each run rebuilds a 20,000-key fixture and
then measures four variants at criterion's default sample size.

Absolute microsecond figures are machine-specific and drift run-to-run on the
documented laptop by up to ~2x, so a fresh pass on other hardware will not
reproduce the published absolutes. The *structure* of the result -- syscall removal
worth ~9x and winning every run, allocation removal indistinguishable from noise --
is the reproducible claim.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
from pathlib import Path

# Criterion group id "read/path-decomposition" becomes this directory name; the
# slash is replaced with an underscore when criterion writes to disk.
GROUP_DIRECTORY = "read_path-decomposition"
BENCH_FILTER = "read/path-decomposition"
BENCH_TARGET = "engine"

# Order matters: `current` is the paired baseline every other variant divides into,
# and the report is emitted in this order to match the README table.
BASELINE = "current"
VARIANTS = (BASELINE, "reused-buffer", "cached-copy", "cached-borrow")

DEFAULT_RUNS = 11
DEFAULT_OUT = Path("benchmarks") / "read-path-decomposition.json"


class BenchError(RuntimeError):
    """A run produced missing, stale, or unusable criterion output.

    Raised rather than skipped. A driver that tolerated a missing variant would
    silently emit a short table, which is the exact defect this script exists to
    remove -- so every guard below aborts the whole pass.
    """


def criterion_root() -> Path:
    """Criterion's output directory, honoring `CARGO_TARGET_DIR` if it is set."""
    target = os.environ.get("CARGO_TARGET_DIR") or "target"
    return Path(target) / "criterion"


def estimates_path(root: Path, variant: str) -> Path:
    """Path to one variant's freshest estimates.

    Criterion rotates the previous run's `new/` into `base/` and writes the current
    run to `new/`, so `new/` is always the run that just finished.
    """
    return root / GROUP_DIRECTORY / variant / "new" / "estimates.json"


def snapshot_mtimes(root: Path) -> dict[str, int | None]:
    """Record each variant's estimates mtime so staleness can be detected later."""
    stamps: dict[str, int | None] = {}
    for variant in VARIANTS:
        path = estimates_path(root, variant)
        stamps[variant] = path.stat().st_mtime_ns if path.exists() else None
    return stamps


def read_median_nanos(path: Path) -> float:
    """Extract the median point estimate, in nanoseconds, from criterion's JSON.

    Parses `estimates.json` rather than scraping stdout: stdout formatting is a
    human-facing concern that criterion is free to change between versions, and it
    rounds to 4-5 significant figures, whereas this file is the machine-readable
    source criterion's own reports are rendered from.

    The median is used rather than the mean because this workload's per-run
    distribution is right-skewed by occasional OS scheduling and page-cache
    stalls; the mean chases those outliers and the median does not.
    """
    try:
        with path.open(encoding="utf-8") as handle:
            document = json.load(handle)
    except FileNotFoundError as error:
        raise BenchError(f"criterion wrote no estimates at {path}") from error
    except json.JSONDecodeError as error:
        raise BenchError(f"{path} is not valid JSON: {error}") from error

    try:
        value = document["median"]["point_estimate"]
    except (KeyError, TypeError) as error:
        raise BenchError(
            f"{path} has no median.point_estimate; criterion output schema changed"
        ) from error

    if not isinstance(value, (int, float)):
        raise BenchError(f"{path}: median.point_estimate is not a number: {value!r}")
    nanos = float(value)
    # A zero or negative duration is impossible and would silently produce an
    # infinite or negative ratio downstream, so reject it here.
    if not (nanos > 0.0) or nanos != nanos or nanos == float("inf"):
        raise BenchError(f"{path}: median.point_estimate is not a positive finite duration: {nanos}")
    return nanos


def run_once(index: int, runs: int, root: Path, extra: list[str]) -> dict[str, float]:
    """Execute one whole-process bench pass and return each variant's median (ns).

    Fails loudly on three distinct conditions, none of which are recoverable into a
    partial table: a non-zero cargo exit, a missing/unparseable estimates file, and
    output that criterion did not rewrite during this pass.
    """
    before = snapshot_mtimes(root)

    command = ["cargo", "bench", "--bench", BENCH_TARGET, "--", BENCH_FILTER, *extra]
    print(f"[{index}/{runs}] {' '.join(command)}", flush=True)
    completed = subprocess.run(command, check=False)
    if completed.returncode != 0:
        raise BenchError(
            f"run {index}/{runs}: `{' '.join(command)}` exited {completed.returncode}"
        )

    medians: dict[str, float] = {}
    for variant in VARIANTS:
        path = estimates_path(root, variant)
        if not path.exists():
            raise BenchError(
                f"run {index}/{runs}: no output for variant {variant!r} at {path}. "
                f"The benchmark filter {BENCH_FILTER!r} matched nothing, or the "
                f"variant was renamed in benches/engine.rs."
            )
        # Staleness guard. Without it, a filter that matched nothing would leave the
        # previous run's files in place and this driver would re-read one run N
        # times -- reporting a plausible table with impossibly tight ranges and a
        # win count of N or 0. That silent-success mode is worse than a crash.
        stamp = path.stat().st_mtime_ns
        if before[variant] is not None and stamp <= before[variant]:
            raise BenchError(
                f"run {index}/{runs}: {path} was not rewritten "
                f"(mtime unchanged at {stamp}). Criterion did not measure "
                f"{variant!r} this pass; refusing to reuse a previous run's numbers."
            )
        medians[variant] = read_median_nanos(path)

    summary = "  ".join(f"{name}={medians[name] / 1000.0:.2f}us" for name in VARIANTS)
    print(f"[{index}/{runs}] {summary}", flush=True)
    return medians


def aggregate(samples: list[dict[str, float]]) -> dict[str, object]:
    """Reduce N per-run measurements into the published table.

    Produces, per variant, the median and min/max of its own timings, plus the
    paired ratio statistics against `current`.
    """
    runs = len(samples)
    if runs == 0:
        raise BenchError("no runs completed; nothing to aggregate")

    variants: dict[str, object] = {}
    for variant in VARIANTS:
        timings_us = [sample[variant] / 1000.0 for sample in samples]
        entry: dict[str, object] = {
            "median_us": round(statistics.median(timings_us), 3),
            "min_us": round(min(timings_us), 3),
            "max_us": round(max(timings_us), 3),
            "runs": runs,
        }
        if variant != BASELINE:
            # Paired per run: the baseline and the variant in a given ratio always
            # come from the same process, so drift cancels.
            ratios = [sample[BASELINE] / sample[variant] for sample in samples]
            entry["paired_ratio_vs_current"] = {
                "median": round(statistics.median(ratios), 3),
                "min": round(min(ratios), 3),
                "max": round(max(ratios), 3),
                "runs_improving": sum(1 for ratio in ratios if ratio > 1.0),
                "runs": runs,
            }
        variants[variant] = entry

    # The zero-copy delta is a ratio between two non-baseline variants, and the
    # README quotes it separately, so it is paired the same way rather than being
    # derived from the two medians above.
    zero_copy = [
        sample["cached-copy"] / sample["cached-borrow"] for sample in samples
    ]
    return {
        "variants": variants,
        "paired_ratio_cached_copy_over_cached_borrow": {
            "median": round(statistics.median(zero_copy), 3),
            "min": round(min(zero_copy), 3),
            "max": round(max(zero_copy), 3),
            "runs_improving": sum(1 for ratio in zero_copy if ratio > 1.0),
            "runs": runs,
        },
    }


def write_artifact(path: Path, payload: dict[str, object]) -> None:
    """Write the artifact as LF-terminated UTF-8 with no BOM.

    Byte-determinism is not incidental here. These artifacts are committed evidence
    diffed across runs and machines, and this repo has already been bitten by a
    PowerShell redirection that produced UTF-16LE with a BOM. `encoding="utf-8"`
    (not `utf-8-sig`) emits no BOM, and `newline="\\n"` stops Python's text layer
    from translating to CRLF on Windows -- matching `.gitattributes`' `* text
    eol=lf` and the `fs::write` in `src/main.rs`.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(payload, handle, indent=1)
        handle.write("\n")


def print_tables(result: dict[str, object]) -> None:
    """Emit the two README tables so the document can be updated by paste."""
    variants: dict[str, dict] = result["variants"]  # type: ignore[assignment]
    runs = variants[BASELINE]["runs"]

    print(f"\n| Variant | Median | Range over {runs} runs | vs. `current` (paired) |")
    print("| --- | ---: | --- | --- |")
    for name in VARIANTS:
        entry = variants[name]
        span = f"{entry['min_us']}-{entry['max_us']} us"
        if name == BASELINE:
            against = "baseline"
        else:
            ratio = entry["paired_ratio_vs_current"]
            against = f"{ratio['median']}x ({ratio['min']}-{ratio['max']}x)"
        print(f"| `{name}` | {entry['median_us']} us | {span} | {against} |")

    print("\n| Ratio | Median | Range | Runs improving |")
    print("| --- | ---: | --- | ---: |")
    for name in VARIANTS[1:]:
        ratio = variants[name]["paired_ratio_vs_current"]
        print(
            f"| `current` / `{name}` | {ratio['median']}x | "
            f"{ratio['min']}-{ratio['max']}x | {ratio['runs_improving']} / {runs} |"
        )
    zero_copy = result["paired_ratio_cached_copy_over_cached_borrow"]
    print(
        f"| `cached-copy` / `cached-borrow` | {zero_copy['median']}x | "
        f"{zero_copy['min']}-{zero_copy['max']}x | "
        f"{zero_copy['runs_improving']} / {runs} |"
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Run the 4-variant read/path-decomposition bench N times and aggregate "
            "per-run paired ratios into the README table."
        )
    )
    parser.add_argument(
        "--runs",
        type=int,
        default=DEFAULT_RUNS,
        help=(
            f"whole-process bench passes to aggregate (default {DEFAULT_RUNS}; the "
            "published table used 11)"
        ),
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=DEFAULT_OUT,
        help=f"artifact path to write (default {DEFAULT_OUT.as_posix()})",
    )
    parser.add_argument(
        "cargo_args",
        nargs="*",
        help="extra arguments appended after the criterion filter",
    )
    arguments = parser.parse_args(argv)

    # One run cannot produce a range or a win count, which is the entire purpose of
    # this driver, so refuse rather than emit a degenerate table.
    if arguments.runs < 2:
        parser.error("--runs must be at least 2; a single run has no range to report")

    root = criterion_root()
    samples: list[dict[str, float]] = []
    try:
        for index in range(1, arguments.runs + 1):
            samples.append(run_once(index, arguments.runs, root, arguments.cargo_args))
    except BenchError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except FileNotFoundError as error:
        # `cargo` itself is not on PATH.
        print(f"error: could not execute cargo: {error}", file=sys.stderr)
        return 1

    # Belt-and-braces: every guard above aborts, so this can only fail if the loop
    # itself is ever changed to continue past a bad run.
    if len(samples) != arguments.runs:
        print(
            f"error: expected {arguments.runs} runs, aggregated {len(samples)}",
            file=sys.stderr,
        )
        return 1

    result = aggregate(samples)
    payload = {
        "artifact_version": 1,
        "producer": "scripts/paired_read_decomposition.py",
        "benchmark": "read/path-decomposition",
        "source": f"benches/{BENCH_TARGET}.rs",
        "command": f"python scripts/paired_read_decomposition.py --runs {arguments.runs}",
        "runs": arguments.runs,
        "keys": 20000,
        "value_bytes": 256,
        "seed": 42,
        "access_order": "deterministic shuffle, identical for all four variants",
        "aggregation": (
            "per-variant median of per-run criterion median point estimates; ratios "
            "computed within each run and then aggregated, so machine-state drift "
            "cancels instead of widening the range"
        ),
        "os": sys.platform,
        "notes": (
            "Absolute timings are machine-specific and drift run-to-run on the "
            "reference laptop by up to ~2x. The reproducible claim is the structure: "
            "removing the read syscall is worth ~9x and wins every run, while "
            "removing the value allocation is indistinguishable from noise."
        ),
        **result,
    }
    write_artifact(arguments.out, payload)
    print_tables(result)
    print(f"\nwrote {arguments.out.as_posix()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
