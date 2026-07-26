---
name: run-lite-os-ui-acceptance
description: Build, launch, and visually verify the LiteOS desktop GUI by driving it with code. Use to run / launch / screenshot / accept / verify the LiteOS UI, desktop, File Manager, Terminal, or compositor — boots the OS in QEMU headless, injects real pointer/keyboard input over QMP, and captures PNG screenshots for inspection.
---

# LiteOS UI 验收（写代码驱动 GUI）

LiteOS 是一个自研内核 + QuickJS + React reconciler 的 OS，GUI 由 Rust 合成器渲染。
`make run-gui` 打开一个 **cocoa 窗口**，需要真实显示器——无头环境（CI、容器、无显示器的
机器）里看不到。**验收 UI 的唯一途径是写代码驱动它**：冷启动 QEMU、通过 QMP 注入真实
virtio-input（移动指针、点击、双击、按键）、再用 QMP `screendump` 抓帧存成 PNG 肉眼核对。

driver 在 `.claude/skills/run-lite-os-ui-acceptance/driver.py`。所有路径相对仓库根。

> ⚠️ **最常见的陷阱：改了 UI 却看到旧界面。** `ui/dist` 必须先构建，再
> `make sync-userland` 同步进 `fs-aarch64.img`。driver 引导的是 `fs-aarch64.img`
> 开发实例镜像，**不是** `target/rootfs/<arch>.img` 只读基线。跳过 sync = 看旧版。

## 前置

- macOS + HVF（本项目默认 `ACCEL=hvf`）；QEMU（`qemu-system-aarch64`，本机 11.0.2）。
- Python 3 + Pillow（driver 用来把 QMP 的 PPM 转 PNG）。缺 Pillow 会退回 `magick`/`convert`，
  再不行就留下 `.ppm` 让你自己转。
- 无需 xvfb/tmux：driver 走 QMP，不开图形窗口。

## 构建 + 同步（改了 UI 必做）

```bash
# 1. 构建 UI bundle（校验 CSS 白名单 + 打包 + 拷图标资源到 ui/dist/）
cd ui && node build.mjs && cd ..

# 2. 构建内核 + bootloader（首次或改了 Rust 才需要；已构建可跳过）
make build ARCH=aarch64

# 3. 把最新 ui/dist 同步进开发实例镜像 fs-aarch64.img
make sync-userland ARCH=aarch64
```

`make sync-userland` 成功时打印 `userland synchronized: N files (<hash>)`；hash 变了说明
内容确实更新。只改 UI（TS/CSS/图标）时，步骤 2 可省，跑 1 + 3 即可。

## 运行（Agent 路径 —— 用这个）

```bash
# 冷启动桌面栈，双击打开 File Manager，抓 3 张验收截图到 /tmp/liteos-ui/
python3 .claude/skills/run-lite-os-ui-acceptance/driver.py --out /tmp/liteos-ui
```

driver 会：
1. 冷启动 QEMU（`-snapshot`：写入落临时 overlay，原镜像只读，不与并行 `make run-gui` 抢写锁）。
2. 等桌面就绪 marker（`compositor: desktop first scene presented`）。
3. 双击桌面图标打开应用（`--open file-manager|terminal`，默认 file-manager），等其 ready marker。
4. 用 QMP 注入点击并 `screendump`，产出 PNG：
   - `opened.png` — 应用刚打开
   - `selected.png` — 单击第一个图标后的选中态
   - `details.png` — 点 Views 按钮切到 Details 视图

**跑完务必用 Read 工具看 PNG**——空白或错误页 = 没真正跑起来。约 30–60s（含冷启动）。

### 验收别的界面 / 交互

driver 的 `main()` 里 shot()/click()/double_click() 是一段直白的脚本，坐标以 **1504x846
逻辑视口的比例**表达（QMP abs 映射 0..0x7FFF，同 `scripts/qemu_gate.py` 的 frame-timing gate）。
要验收新界面或新交互流，照抄该序列改坐标即可。可用的操作：

- `move(xf, yf)` — 移动绝对指针到视口比例位置（触发 hover）
- `click(xf, yf)` — 单击（选中 / 按钮）
- `double_click(xf, yf)` — 双击（打开）
- `shot("name")` — screendump 当前帧到 `<out>/name.png`

`qmp`（`scripts/qemu_gate.py:QmpClient`）还提供 `button(name, down)` 与 `move_abs`；
按键注入可加 `{"type":"key", ...}` 的 `input-send-event`（QmpClient._send_events）。

## 运行（人工路径 —— 需真实显示器）

```bash
make run-gui ARCH=aarch64        # 打开 cocoa 窗口；Ctrl-C 退出。无头环境无用。
```

## Gotchas（实战踩坑）

- **两个镜像别搞混。** `fs-aarch64.img`（8GB，`make run`/`sync` 写入的开发实例）vs
  `target/rootfs/<arch>.img`（128MB 只读基线，frame gate 用）。driver 引导前者。UI 改动看不到
  基本都是同步进了错的镜像 / 没同步。
- **镜像写锁。** 若已有 `make run-gui` 在跑，它锁着 `fs-aarch64.img`，直接引导会
  `Failed to get "write" lock`。driver 用 `-snapshot` 规避（原镜像只读打开）。真冲突时先
  `pkill -f "qemu-system-aarch64.*fs-aarch64"`。
- **UI 字体是固定位图图集，非 TTF。** 只覆盖 ASCII + U+FFFD + GB2312（汉字/符号行）。缺字
  渲染成 U+FFFD 方块。UI 里用非 ASCII 字符前先确认在图集内——如 `▲`(U+25B2) 在、
  `▼`(U+25BC) **不在**；chevron 用 `∧`/`∨`（都在）。生成脚本见 `scripts/generate_ui_font.py`
  的 `codepoints()`。
- **flex 图标网格换行需容器有确定宽度。** taffy 靠父级 resolved width 断行；网格容器给
  `width:100%`（在 `flex:1` 撑满的祖先里即确定宽度），子项用 `width` **不用 `flex`**
  （`flex` 简写会置 `flex-basis:0`+`shrink:1` 压扁定宽项）。
- **文本 `text-overflow:ellipsis` 对居中定宽标签的舍入敏感。** 标签用确定 `width`（如
  `width:100%`）而非 `max-width` + shrink-to-fit，否则测量舍入会让短名（"bin"）误触发省略号。
- **CSS 校验器禁 `* @ :: [ ]` 与逗号选择器**（`ui/build.mjs` 的 `validateCss`），所以
  `/* */` 注释也会被拒。改 CSS 后先 `cd ui && node build.mjs --check` 抓错。

## Troubleshooting

| 症状 | 原因 / 修复 |
|---|---|
| `RuntimeError: QEMU exited during desktop boot` + `Failed to get "write" lock` | 有别的 QEMU 锁着镜像。`pkill -f "qemu-system-aarch64.*fs-aarch64"` 后重跑。 |
| `missing .../fs-aarch64.img` | 先 `make sync-userland ARCH=aarch64`（它从 `target/rootfs` 基线派生 + 同步 dist）。 |
| 截图是**旧版** UI | 没 `make sync-userland`，或改了 UI 没先 `cd ui && node build.mjs`。 |
| `timed out during ... launch; missing=[...]` | 应用没起（看打印的串口 tail）。常见：dist 里图标文件名与 `build.mjs` 的 copyFile 目标不符 → 应用崩在缺资源上。 |
| PNG 存成了 `.ppm` | 无 PNG 转换器。`pip install Pillow` 或装 ImageMagick。 |
