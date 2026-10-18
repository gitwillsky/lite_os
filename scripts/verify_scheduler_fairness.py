#!/usr/bin/env python3
"""验证 N+1 个 CPU 密集任务在 N 个 CPU 上得到公平的 CPU 时间。

任务被抢占后按负载重新选核，但负载并列时会留在原 CPU；没有周期性轮转时，N+1 个任务的分布会永远锁死为
“几个任务独占一个 CPU，另外两个对半分一个 CPU”（100% / 50% / 50%）。本 gate 在 guest 里启动 N+1 个忙循环，
读每个循环的 utime，要求最少者至少达到最多者的 `MIN_FAIR_PERCENT`%。锁死的分布比值是 50%，公平分布接近 100%，
阈值取在两者之间并留出宿主调度抖动的余量。

计时取自 guest 内核的 /proc/<pid>/stat，与宿主墙钟无关。
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from build_cache import publish_runtime_gate, runtime_gate_hit, runtime_gate_payload
from build_target import BuildTarget, target_from_environment
from qemu_gate import SHELL_PROMPT, ShellCommand, boot

ROOT = Path(__file__).resolve().parent.parent

# (guest CPU 数, 忙循环数)：2 CPU 上的 3 个任务是最小的 N+1 情形；4 CPU 上的 5 个任务覆盖更大的 N。
CONFIGURATIONS = ((2, 3), (4, 5))
# 每个配置让忙循环运行的秒数；公平分布在几十个轮转周期内收敛。
RUN_SECONDS = 6
# 最少者占最多者的百分比下限。锁死分布为 50，公平分布接近 100。
MIN_FAIR_PERCENT = 70
# Bump to force a re-run after changing the workload or the threshold.
RECIPE_VERSION = 1


def default_image(target: BuildTarget) -> Path:
    """Returns the target-isolated read-only rootfs baseline."""
    return ROOT / "target" / "rootfs" / f"{target.arch}.img"


def marker(cpus: int, tasks: int, verdict: int) -> str:
    """Returns the serial marker the guest prints for one configuration and verdict."""
    return f"SCHED_FAIR_VERDICT_{cpus}_{tasks}_{verdict}"


def fairness_command(cpus: int, tasks: int) -> bytes:
    """Builds the guest shell command for one configuration.

    The verdict is computed by the guest shell. The typed command is echoed to the serial line, so
    the marker must never appear literally in it: it only contains `$verdict`, whose value exists
    after the workload ran.
    """
    return (
        "pids=''; for i in $(seq 1 " + str(tasks) + "); do "
        "sh -c 'while :; do :; done' & pids=\"$pids $!\"; done; "
        "sleep " + str(RUN_SECONDS) + "; min=999999; max=1; "
        "for p in $pids; do t=$(cut -d' ' -f14 /proc/$p/stat); "
        "[ $t -lt $min ] && min=$t; [ $t -gt $max ] && max=$t; done; kill $pids; "
        "verdict=$((min*100/max >= " + str(MIN_FAIR_PERCENT) + ")); "
        "echo SCHED_FAIR_STATS_" + f"{cpus}_{tasks}" + " min=$min max=$max; "
        "echo SCHED_FAIR_VERDICT_" + f"{cpus}_{tasks}" + "_$verdict\n"
    ).encode()


def gate_inputs(target: BuildTarget, image: Path) -> tuple[Path, ...]:
    """Cache inputs: every artifact that can change the scheduler's observable behavior."""
    inputs = [
        image,
        ROOT / target.kernel_elf(),
        ROOT / target.kernel_boot_artifact(),
        ROOT / "scripts" / "qemu_gate.py",
        Path(__file__).resolve(),
    ]
    if target.requires_bootloader:
        inputs.append(
            ROOT / "bootloader" / "target" / target.kernel_triple / "release" / "bootloader"
        )
    return tuple(inputs)


def main() -> int:
    target = target_from_environment()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--image",
        type=Path,
        default=default_image(target),
        help="read-only baseline rootfs; every boot works on a private copy",
    )
    args = parser.parse_args()
    if target.arch != "aarch64":
        print(f"scheduler fairness gate skipped: {target.arch} keeps compile/static/boot coverage")
        return 0
    image = args.image.resolve()
    try:
        if not image.is_file():
            raise RuntimeError(f"rootfs image is missing: {image}")
        stamp = ROOT / "target" / "verify-gates" / f"scheduler-fairness-{target.arch}.json"
        payload = runtime_gate_payload("scheduler-fairness", RECIPE_VERSION, gate_inputs(target, image))
        if runtime_gate_hit(stamp, payload, (image,)):
            print("scheduler fairness verification cache hit")
            return 0
        for cpus, tasks in CONFIGURATIONS:
            boot(
                image,
                cpus,
                (marker(cpus, tasks, 1),),
                timeout_seconds=RUN_SECONDS + 60,
                interactions=((SHELL_PROMPT, ShellCommand(fairness_command(cpus, tasks))),),
                forbidden_markers=(marker(cpus, tasks, 0),),
            )
            print(f"scheduler fairness: {tasks} tasks on {cpus} CPUs passed")
        publish_runtime_gate(stamp, payload)
    except RuntimeError as error:
        print(f"scheduler fairness verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
