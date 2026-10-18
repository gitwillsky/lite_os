#!/usr/bin/env python3
"""量化多 CPU 调度的两个延迟指标；这是诊断工具，不判定通过与否，也不属于 `make verify`。

调度器只有“任务被抢占后重新选核”这一种负载均衡（见 docs/architecture/process-scheduling.md），没有空闲
CPU 主动拉取或推送。本工具回答：这个缺口在实践里有多大？

实验 L（lag）：C 退出使 CPU1 空闲的瞬间，排队在 CPU0 上的 B 要多久才出现在 CPU1。直接测量“空闲 CPU 等到
排队任务”的延迟。

实验 M（make）：一次启动 3N 到 4N 个长度随机的 CPU 密集任务，测总耗时，并与同一任务集在**理想处理器共享**
下的总耗时逐次配对比较。理想值是零迁移延迟、永远工作守恒的公平调度；时间片轮转本身就达不到理论下界，
所以不能拿实测值直接与下界比，必须和理想基线比。额外损失包含 fork、唤醒、tick 粒度等全部开销，因此是均衡
延迟造成损失的**上限**。

计时取自 guest 内核的单调时钟与 CPU 时间；在 TCG 下的结果不具代表性。
"""

from __future__ import annotations

import argparse
import re
import shutil
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

from build_target import acceleration_from_environment, target_from_environment
from ext4_image import recover_ext4_journal, run_debugfs
from qemu_gate import SHELL_PROMPT, ShellCommand, boot
from verify_musl import cached_musl_paths, compile_static_guest_program, find_compiler

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "scripts/fixtures/diagnostics/scheduler-latency.c"
GUEST_PATH = "/var/tmp/scheduler-latency"
DONE_MARKER = "SCHED_LAT_DONE"

LAG_CPUS = 2
LAG_TRIALS = 40
# (guest CPU 数, 任务数)
MAKE_CONFIGURATIONS = ((2, 6), (4, 12), (4, 16))
MAKE_TRIALS = 25
# 与 C 程序 trial_seed 一致。
SEED_BASE = 12345
SEED_STEP = 7919


def trial_seed(trial: int) -> int:
    """Returns the seed the guest uses for one make-mode trial."""
    return SEED_BASE + trial * SEED_STEP


def job_lengths_ms(seed: int, jobs: int) -> list[int]:
    """Replicates the guest's `job_lengths`: a 32-bit LCG, 20..119 ms of CPU time per job."""
    state = seed & 0xFFFFFFFF
    lengths = []
    for _ in range(jobs):
        state = (state * 1664525 + 1013904223) & 0xFFFFFFFF
        lengths.append(20 + (state >> 8) % 100)
    return lengths


def ideal_makespan_ms(lengths: list[int], cpus: int) -> float:
    """Total time of ideal processor sharing: k active jobs each run at rate min(1, cpus / k).

    There is no migration cost and no idle CPU while enough jobs remain, so this is what a perfect
    fair scheduler achieves on the same job set. It is above the `max(sum / cpus, longest)` bound
    whenever the last jobs finish unevenly.
    """
    remaining = sorted(float(length) for length in lengths)
    elapsed = 0.0
    while remaining:
        rate = min(1.0, cpus / len(remaining))
        shortest = remaining[0]
        elapsed += shortest / rate
        remaining = [r - shortest for r in remaining[1:] if r - shortest > 1e-9]
    return elapsed


def percentile(values: list[float], percent: int) -> float:
    """Nearest-rank percentile of a non-empty list."""
    ordered = sorted(values)
    rank = max(1, -(-len(ordered) * percent // 100))
    return ordered[rank - 1]


@dataclass(frozen=True)
class MakeTrial:
    cpus: int
    jobs: int
    seed: int
    makespan_us: int


TRIAL_RE = re.compile(
    r"SCHED_LAT_TRIAL make cpus=(\d+) jobs=(\d+) seed=(\d+) makespan_us=(\d+)"
)
LENGTHS_RE = re.compile(r"SCHED_LAT_RESULT lengths seed=(\d+) jobs=(\d+) ms=([\d,]+)")
LAG_RE = re.compile(
    r"SCHED_LAT_RESULT lag n=(\d+) timeouts=(\d+) min_us=(\d+) p50_us=(\d+) "
    r"p90_us=(\d+) max_us=(\d+) mean_us=(\d+)"
)


def parse_make_trials(serial: str) -> list[MakeTrial]:
    """Extracts every make-mode trial from guest serial output."""
    return [MakeTrial(*map(int, match.groups())) for match in TRIAL_RE.finditer(serial)]


def extra_loss_percent(trial: MakeTrial) -> float:
    """Percent by which one measured makespan exceeds ideal processor sharing for its own job set."""
    ideal_us = ideal_makespan_ms(job_lengths_ms(trial.seed, trial.jobs), trial.cpus) * 1000
    return (trial.makespan_us / ideal_us - 1.0) * 100.0


def default_image() -> Path:
    return ROOT / "target" / "rootfs" / f"{target_from_environment().arch}.img"


def run_guest(image: Path, binary: Path, cpus: int, arguments: str, timeout_seconds: int) -> str:
    """Boots a private copy of the image with `binary` installed, runs it once, returns the serial text."""
    with tempfile.TemporaryDirectory(prefix="liteos-sched-diag-") as directory:
        private = Path(directory) / "rootfs.img"
        shutil.copyfile(image, private)
        recover_ext4_journal(private)
        run_debugfs(private, f"write {binary} {GUEST_PATH}", writable=True)
        # The kernel requires at least one execute bit for root execve.
        run_debugfs(private, f"set_inode_field {GUEST_PATH} mode 0100755", writable=True)
        return boot(
            private,
            cpus,
            (DONE_MARKER,),
            timeout_seconds=timeout_seconds,
            interactions=((SHELL_PROMPT, ShellCommand(f"{GUEST_PATH} {arguments}\n".encode())),),
            persistent_writes=True,
        )


def check_random_generators_agree(image: Path, binary: Path) -> None:
    """Fails if the host and guest disagree on a job set; every ideal baseline depends on it."""
    seed, jobs = trial_seed(0), 12
    serial = run_guest(image, binary, 1, f"lengths {seed} {jobs}", 90)
    match = LENGTHS_RE.search(serial)
    if match is None:
        raise RuntimeError("guest printed no job lengths")
    guest = [int(value) for value in match.group(3).split(",")]
    if guest != job_lengths_ms(seed, jobs):
        raise RuntimeError(f"guest job lengths {guest} differ from host {job_lengths_ms(seed, jobs)}")


def measure_lag(image: Path, binary: Path) -> dict[str, int]:
    """Runs experiment L and returns its summary fields in microseconds."""
    serial = run_guest(image, binary, LAG_CPUS, f"lag {LAG_TRIALS}", 300)
    match = LAG_RE.search(serial)
    if match is None:
        raise RuntimeError("guest printed no lag summary")
    keys = ("n", "timeouts", "min_us", "p50_us", "p90_us", "max_us", "mean_us")
    return dict(zip(keys, map(int, match.groups())))


def measure_make(image: Path, binary: Path, cpus: int, jobs: int) -> list[MakeTrial]:
    """Runs experiment M for one configuration and returns every trial."""
    serial = run_guest(image, binary, cpus, f"make {cpus} {jobs} {MAKE_TRIALS}", 600)
    trials = parse_make_trials(serial)
    if len(trials) != MAKE_TRIALS:
        raise RuntimeError(f"expected {MAKE_TRIALS} make trials, guest printed {len(trials)}")
    return trials


def print_lag(summary: dict[str, int]) -> None:
    print(f"idle CPU waiting for a queued task ({LAG_CPUS} CPUs, {summary['n']} trials, {summary['timeouts']} timeouts):")
    print(
        f"  min {summary['min_us'] / 1000:.2f} ms   p50 {summary['p50_us'] / 1000:.2f} ms   "
        f"p90 {summary['p90_us'] / 1000:.2f} ms   max {summary['max_us'] / 1000:.2f} ms   "
        f"mean {summary['mean_us'] / 1000:.2f} ms"
    )


def print_make(trials: list[MakeTrial]) -> None:
    losses = [extra_loss_percent(trial) for trial in trials]
    print(f"burst of {trials[0].jobs} CPU-bound jobs on {trials[0].cpus} CPUs ({len(trials)} paired trials):")
    print(
        "  extra time over ideal processor sharing   "
        f"p50 {percentile(losses, 50):+.1f}%   p90 {percentile(losses, 90):+.1f}%   max {max(losses):+.1f}%"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, default=default_image(), help="read-only baseline rootfs")
    args = parser.parse_args()
    image = args.image.resolve()
    if not image.is_file():
        print(f"rootfs image is missing: {image}", file=sys.stderr)
        return 1
    if acceleration_from_environment() != "hvf":
        print("warning: not running under HVF; timing results are not representative", file=sys.stderr)
    try:
        with tempfile.TemporaryDirectory(prefix="liteos-sched-bin-") as directory:
            binary = Path(directory) / "scheduler-latency"
            compiler = find_compiler()
            compile_static_guest_program(compiler, cached_musl_paths(compiler), [SOURCE], binary)
            check_random_generators_agree(image, binary)
            print_lag(measure_lag(image, binary))
            for cpus, jobs in MAKE_CONFIGURATIONS:
                print_make(measure_make(image, binary, cpus, jobs))
    except RuntimeError as error:
        print(f"scheduler latency diagnosis failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
