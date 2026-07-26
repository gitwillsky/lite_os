---
name: analyze-with-codex
description: Delegate hard problem analysis, root-cause confirmation, or solution review to Codex as an independent second opinion. Use for a tough bug whose root cause you cannot confirm alone, or before committing an architecture-level fix you want independently reviewed. A slow (multi-minute) read-only background task — NOT for simple/local changes or conclusions you already trust. Triggers: analyze / root cause / review my fix / second opinion / cross-check with Codex.
---

# 让 Codex 协助分析 / 根因确认 / 方案评审

把疑难问题交给 **Codex** 做独立第二意见:它在**只读沙箱**里读代码分析,验证/纠正你的根因假设,
或评审你的根修方案。驱动脚本 `.claude/skills/analyze-with-codex/driver.py`。路径相对仓库根。

> ⏳ **这是后台耗时较长（数分钟）的任务,会消耗额外算力。发起前先过一遍下面的使用纪律。**

## 何时用（务必自问,防滥用）

**该用**——满足其一：
- 疑难 bug 你已自查、加过调试、但**根因仍不确定**,需要独立视角**交叉验证**;
- 有**架构取舍/风险**的根修方案,提交前想要一次独立**方案评审**;
- 分析要**跨多个子系统**,单一上下文难一次看全。

**别用**（直接自己做）：
- 简单/局部改动、你已有把握的结论;
- 读几个文件就能直接确认的问题;
- 赶时间的小修。**不要为"求稳"把每个决定都例行外包**——每次数分钟 + 算力成本。

本会话正例:光标 resize 残留 bug——我已逐像素调试锁定嫌疑,但涉及 compositor 输入路由的架构级
根因,请 Codex 独立验证 + 给根修方案,交叉确认后才动手。这是"该用"的典型。

## 前置
- `codex` CLI（本机 codex-cli 0.145.0）。
- 只读分析:`--sandbox read-only`,Codex 不改任何文件。

## 发起分析（Agent 路径 —— 用这个）

**prompt 必须自带完整证据链**——Codex 看不到你的对话上下文。结构:现象 / 已确认事实（**file:line**）/
你的根因假设（请它验证或纠正）/ 明确要它回答什么（+ 提醒遵守 AGENTS.md）。证据越具体,结论越可信。

**务必后台跑**（`run_in_background: true`,或 shell `&`）——`codex exec` 数分钟,会超时转后台;
收到完成通知后再读结果。`--out` 把完整输出落盘：

```bash
python3 .claude/skills/analyze-with-codex/driver.py analyze \
    --out /tmp/codex-analysis.md \
    --prompt "$(cat <<'EOF'
分析并确认某 bug 的根因，给根修方案。只分析不改代码，输出根因结论 + 具体修改（文件:行 + 代码）。
## 现象
（复现路径 + 观察到的错误行为，越具体越好）
## 已确认事实（file:line）
（你走查/调试得到的、带文件行号的事实）
## 我的根因假设（请验证或纠正）
（你的判断，让 Codex 印证或推翻）
## 请你回答
1) 验证/纠正根因；2) 根修方案（文件:行 + 代码），评估取舍与最简形态、无双轨；
3) 遵守 AGENTS.md（最小改动、单一 owner、契约同步、中文注释）。
EOF
)"
```

底层等价于本会话实测跑通的：`codex exec --sandbox read-only --cd <repo> "<prompt>"`。

## 追问（同一会话补细节）

Codex 首轮结论可能不够细（或你只读到部分）。用 `ask` 对**上一次会话**追问,保留其分析上下文：

```bash
python3 .claude/skills/analyze-with-codex/driver.py ask \
    --out /tmp/codex-followup.md \
    --prompt "用简洁中文重述你方案里 X 处的具体代码 diff（文件:行 + 片段）。"
```

底层：`codex exec --sandbox read-only resume --last "<prompt>"`（本会话用它追要过具体 diff）。

## 读结果

**用 Read 工具读 `--out` 整个文件,不要用 `tail`/`head`**——本会话踩过坑:`tail` 只截尾部,
且 resume 会覆盖同一输出,导致分析前半部分（根因、字段设计）丢失。落盘 + Read 全文才完整。
读到结论后,**交叉对比你自己的判断**再决定是否采纳——Codex 是第二意见,不是最终裁决。

## Gotchas（本会话实战）

- **prompt 无上下文**:Codex 不知道你和用户聊了什么,证据链（现象/file:line/假设）必须写全,
  否则它只能泛泛而谈。
- **后台 + 耗时**:同步等会超时转后台;直接后台跑,靠完成通知回收。别在等待时空转。
- **`tail` 会骗你**:大段分析被截断看着"没结论";务必 `--out` 落盘 + Read 全文。
- **resume flag 顺序**:`codex exec --sandbox read-only resume --last "<prompt>"`——`--sandbox`
  在 `resume` 之前,放后面会报 `unexpected argument '--sandbox'`（本会话报过）。
- **只读**:分析用 `read-only`,Codex 给方案但不改代码;改由你按 AGENTS.md 亲自落地并验证。
- **第二意见非圣旨**:采纳前和自己的走查/证据对齐;有分歧就再 `ask` 追问,别盲从。

## Troubleshooting

| 症状 | 修复 |
|---|---|
| `codex: command not found` | 装/激活 codex CLI（本机在 fnm shell 的 bin 里）。 |
| `unexpected argument '--sandbox'` (resume) | flag 放到 `resume` 之前:`codex exec --sandbox read-only resume --last ...`。 |
| 输出看着没结论/被截断 | 你在用 tail/head;改用 `--out` 落盘 + Read 读整个文件。 |
| Codex 结论太泛、答非所问 | prompt 缺证据链;补现象 + file:line 事实 + 明确问题重发。 |
| 同步命令超时 | 正常——`codex exec` 耗时数分钟;改后台跑,收到完成通知再读 `--out`。 |
