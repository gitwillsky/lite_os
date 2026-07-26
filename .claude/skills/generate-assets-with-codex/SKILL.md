---
name: generate-assets-with-codex
description: Generate image assets (icons, cursors, sprites, PNGs) for LiteOS by delegating to Codex, then validate size/format and preview them. Use whenever you need to create / draw / generate / redraw icons, cursors, sprites, or any PNG art for the desktop, apps, or compositor — Codex draws the images from a requirements-only prompt, and driver.py validates dimensions/RGBA and builds a magnified preview for eyeballing.
---

# 用 Codex 生成图片素材

LiteOS 的图标/光标/精灵等 PNG 素材**不手写、不程序化像素绘制**，而是委托 **Codex** 生成
（它自己知道怎么画），再用 `.claude/skills/generate-assets-with-codex/driver.py` 校验尺寸/格式
并拼放大预览肉眼检查。生成出的源 PNG 放 `assets/<name>-src/`，随后由**该素材各自的打包/拷贝
步骤**转成项目消费格式（见下"接入项目"）。所有路径相对仓库根。

## 何时用

- 需要新的或重绘的图标、光标、精灵、任何 PNG 美术素材（如"把文件管理器图标做成 XP 风格"、
  "鼠标指针高清重绘"）。
- 用户明确要求"用 Codex 生成/重绘"素材，而不是程序化绘制。

## 前置

- `codex` CLI（本机 codex-cli 0.145.0）。
- Python 3 + Pillow（driver.py 的 check 用来校验与拼预览）。

## 生成（Agent 路径 —— 用这个）

**核心规则:prompt 只描述"图片要求",绝不指导 Codex 怎么画**（它自己会用合适工具生成；
指导实现反而画蛇添足——本会话踩过这个坑）。要求里务必写清:每张图的**文件名 + 精确像素
尺寸**、**透明背景 / RGBA**、风格、每张的语义（朝向、热点位置等）、"在其尺寸内清晰居中留边距"。

直接用 `codex exec`（本会话生成 XP 文件管理器图标与 XP 光标都用此形态，均成功）：

```bash
codex exec --sandbox workspace-write --cd assets/cursors-src "在当前目录生成一组忠实还原 \
Windows XP (Luna) 风格的鼠标指针 PNG。每个 64x64、RGBA、透明背景、边缘抗锯齿、白填充+黑描边+\
轻微柔和投影。需要：arrow.png(64x64) 经典左上箭头，尖端精确落在左上角像素（热点）；\
pointer.png(64x64) 手型，指尖在左上；resize-ns.png(64x64) 上下双箭头，居中对称。\
完成后确认每个是对应尺寸的 RGBA PNG。"
```

- `--cd <目标目录>` 把 Codex 的写入限制在该目录；先 `mkdir -p assets/<name>-src`。
- `--sandbox workspace-write` 让它能写文件。
- **生成较慢（几十秒~数分钟）**：放后台跑（`run_in_background: true` 或 shell `&`），
  收到完成通知后再校验。用 Monitor 等"N 个文件出现"也可。

driver.py 的 `gen` 子命令封装了同一 `codex exec` 调用（前台阻塞版，便于脚本化）：

```bash
python3 .claude/skills/generate-assets-with-codex/driver.py gen \
    --dir assets/my-icons-src \
    --prompt '生成一组 XP 风格图标 PNG，透明背景 RGBA 边缘抗锯齿。需要：folder.png(32x32) 黄色\
              文件夹；file.png(32x32) 白色文档带折角。每张清晰居中留 1px 边距。'
```

## 校验 + 预览（生成完成后必做）

**尺寸/模式对不代表图形对**——必须肉眼看放大预览（形状、朝向、透明边缘）：

```bash
python3 .claude/skills/generate-assets-with-codex/driver.py check \
    --dir assets/cursors-src \
    --spec 'arrow.png=64x64' --spec 'pointer.png=64x64' --spec 'resize-ns.png=64x64' \
    --preview /tmp/preview.png
```

输出每张的 `OK/BAD  name (w,h) mode`（校验精确尺寸 + RGBA），并把所有图 6× NEAREST 放大、
铺棋盘背景拼成 `/tmp/preview.png`。**然后用 Read 工具打开该 PNG 亲眼确认**——空白、错位、
边缘发灰(未预乘/杂边)、朝向反了都要在这一步抓出来。`--scale`/`--cols` 可调。

## 接入项目（生成后如何变成项目资源）

源 PNG 只是原料，还需转成项目消费格式。跟随已有先例：

- **直接拷 PNG**：`ui/build.mjs` 的 file-manager 块把 `assets/sprites-src/*.png`
  `copyFile` 进 `ui/dist/file-manager/assets/`（图标名须与代码里 `src=` 一致，否则静默不绘制）。
- **打包成自定义格式**：`scripts/pack_cursor_assets.py` 把 `assets/cursors-src/*.png`
  预乘后打包成 compositor 的 `.lc2`（磁盘字节序 `[B,G,R,A]`，over() 要预乘）。新素材若有专用
  二进制格式，照此写一个打包脚本，删掉被取代的旧程序化生成脚本（AGENTS.md 禁孤儿/双轨）。

## Gotchas（本会话实战）

- **prompt 别指导实现**：写"需要 64x64 RGBA 透明背景的 XP 箭头"，别写"用 Pillow 超采样再阈值"。
  Codex 自己会画；越俎代庖会得到更差结果。（用户当场纠正过。）
- **生成是异步/慢**：`codex exec` 几十秒起步，务必后台跑 + 完成通知/Monitor，别同步干等。
- **必看预览**：Codex 偶尔尺寸对但形状/朝向不符要求；`check` 的放大预览是唯一拦截点。
- **文件名即契约**：下游 `copyFile`/打包脚本按名字取图，错名 → 构建拷贝报错或运行时静默不绘制。
- **透明/预乘**：直接当 `<img>` 用的 PNG 保持直 alpha；喂给预乘合成器（如 `over()`）的要在打包
  脚本里预乘，否则边缘过亮。
- **首次生成到目标目录前先 `mkdir -p`**：`--cd` 指向不存在的目录 codex 可能报错。

## Troubleshooting

| 症状 | 修复 |
|---|---|
| `codex: command not found` | 装/激活 codex CLI（本机在 fnm shell 的 bin 里）。 |
| check 报 `需要 Pillow` | `pip install Pillow`。 |
| check 全 `BAD ... 缺失` | Codex 还没生成完，或 `--dir` 指错；确认 codex exec 已结束、目录正确。 |
| 预览里图形发灰/有杂边 | Codex 输出的透明边缘未干净，或后续该预乘却没预乘；重生成或在打包脚本预乘。 |
| 尺寸 BAD | prompt 没写死精确像素，或 Codex 画大了；prompt 里逐张写 `name(WxH)` 并要求"确认为该尺寸"。 |
