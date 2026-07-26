#!/usr/bin/env python3
"""让 Codex 作为独立第二意见，协助问题分析 / 根因确认 / 方案评审的 harness。

本会话实测跑通（光标残留 bug：请 Codex 独立验证根因 + 给根修方案 + resume 追问具体 diff）。
Codex 在**只读沙箱**里读代码分析，不改任何文件。这是一个**后台耗时较长**（数分钟）的任务。

⚠️ 使用纪律（防滥用）——仅在下列场景发起：
  - 疑难 bug 你已自查但根因不确定，需要独立视角**交叉验证**根因；
  - 有架构取舍/风险的根修方案，提交前想要一次独立**方案评审**；
  - 跨多个子系统、单一上下文难以一次看全的分析。
  不要用于：简单/局部改动、你已有把握的结论、能靠读几个文件直接确认的问题、
  赶时间的小修——那种直接自己做，别为"求稳"而例行外包（浪费数分钟与算力）。

两步：
  analyze —— 发起一次只读分析（后台）。prompt 必须自带**完整证据链**：现象、
             已确认事实（file:line）、你的根因假设（请它验证或纠正）、要它回答什么。
  ask     —— 对上一次会话追问（codex exec resume --last），补细节 / 要具体 diff。

用法：
  # 1. 发起分析（务必后台跑：run_in_background 或 shell &）。--out 保存完整输出。
  python3 .claude/skills/analyze-with-codex/driver.py analyze \\
      --out /tmp/codex-analysis.md \\
      --prompt "$(cat <<'EOF'
  分析并确认某 bug 的根因，给根修方案。只分析不改代码。
  ## 现象
  ...
  ## 已确认事实（file:line）
  ...
  ## 我的根因假设（请验证或纠正）
  ...
  ## 请你回答
  1) 验证/纠正根因；2) 给根修方案（文件:行+代码），评估取舍；3) 注意 AGENTS.md 约束。
  EOF
  )"

  # 2. 追问（同一 Codex 会话，补具体 diff / 边角）
  python3 .claude/skills/analyze-with-codex/driver.py ask \\
      --out /tmp/codex-followup.md \\
      --prompt "重述你方案里 X 处的具体代码 diff（文件:行+片段），我丢失了前半部分。"

完整输出写入 --out 文件（**不要用 `tail` 看**——会截断，本会话踩过；用 Read 工具读整个文件）。
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]


def run_codex(cmd: list[str], out: Path | None) -> int:
    """跑 codex，若给了 --out 则把 stdout+stderr 完整落盘（供 Read 工具读全）。"""
    if out is None:
        return subprocess.run(cmd, cwd=ROOT).returncode
    out.parent.mkdir(parents=True, exist_ok=True)
    with out.open("wb") as handle:
        proc = subprocess.run(cmd, cwd=ROOT, stdout=handle, stderr=subprocess.STDOUT)
    print(f"Codex 输出已完整写入 {out}（用 Read 工具读整个文件，勿用 tail）")
    return proc.returncode


def cmd_analyze(args: argparse.Namespace) -> int:
    # read-only：Codex 只读代码做分析，绝不改文件。--cd 限定它看本仓库。
    cmd = [
        "codex",
        "exec",
        "--sandbox",
        "read-only",
        "--cd",
        str(ROOT),
        args.prompt,
    ]
    return run_codex(cmd, Path(args.out) if args.out else None)


def cmd_ask(args: argparse.Namespace) -> int:
    # 对最近一次 Codex 会话追问，保留其分析上下文。注意 flag 在 resume 之前。
    cmd = [
        "codex",
        "exec",
        "--sandbox",
        "read-only",
        "resume",
        "--last",
        args.prompt,
    ]
    return run_codex(cmd, Path(args.out) if args.out else None)


def main() -> int:
    parser = argparse.ArgumentParser(description="Codex 只读分析 / 根因 / 方案评审 harness")
    sub = parser.add_subparsers(dest="mode", required=True)

    a = sub.add_parser("analyze", help="发起一次只读分析（后台耗时任务）")
    a.add_argument("--prompt", required=True, help="含完整证据链的分析请求")
    a.add_argument("--out", help="完整输出落盘路径（推荐；用 Read 读，勿用 tail）")
    a.set_defaults(func=cmd_analyze)

    k = sub.add_parser("ask", help="对上一次 Codex 会话追问 (resume --last)")
    k.add_argument("--prompt", required=True, help="追问内容")
    k.add_argument("--out", help="完整输出落盘路径")
    k.set_defaults(func=cmd_ask)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
