# ADR: UI 交互类 runtime gate 在 UI 重构前移除

- 状态：已接受
- 日期：2026-10-18

## 背景

audio gate 与 frame-timing gate 都经 QMP 注入固定屏幕坐标的点击和滚轮，驱动真实 UI（Command Center、
System Center、Music、窗口 resize/scroll/move）。UI 布局一变，坐标就失效；UI 架构即将重构。

在当前 UI 上，frame-timing 的 move 阶段因合成器仍按已关闭面板的过期场景路由，把标题栏按下落到全屏遮罩上，
`MOVE_BEGIN` 被拒（经合成器插桩确认）；延长等待后 scroll 阶段帧间隔超过阈值。audio gate 在 System Center
静音步骤缺少预期 marker，原因未查明，但该步骤同样依赖固定坐标点击。两个门禁的红灯使 `make verify` 无法作为
内核正确性的裁决。

## 决策

- 移除 `verify_audio.py`、`verify_frame_timing.py`、QMP 客户端与输入/帧负载驱动、WAV 抓取，以及依赖它们的
  UI 验收 skill。
- 内核显示/GPU 改由不依赖 UI 的 `verify_gpu.py` 裁决：`drm-flip-probe` 直接走 VirGL2、EXECBUFFER、
  PAGE_FLIP 路径，验证像素读回、flip 事件顺序与 vblank 间隔（见 build-and-verify 性能测试段）。
- `scripts/tests` 的 Python 测试并入 `verify-unit`，使门禁脚本自身的固定值与判定逻辑不会无人运行而过期。

## 结果

- 旧图形用户态整体移除，当前没有 UI 交互或音频端到端 runtime 覆盖。
- 内核 page-flip、像素读回与 GPU 节奏仍由独立探针覆盖。
- 新桌面按 Wayland 设计实现后重建 UI 门禁，音频混音另需多路真实输出验证；两者均不依赖固定屏幕坐标。
