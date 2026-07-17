# 显示 damage 不变量与残影诊断

本文沉淀软件合成器、DRM `DIRTYFB` 与 VirtIO-GPU 局部更新的共同模型和诊断顺序。
当前实现事实见
[display-terminal architecture](../architecture/display-terminal.md)，稳定约束见
[display-terminal contract](../architecture-contract/display-terminal.md)。

## 1. 正确性模型

显示链路按以下顺序传递同一组半开像素区域：

```text
scene mutation
  -> old/new visual bounds
  -> CPU framebuffer raster
  -> DRM DIRTYFB clips
  -> VirtIO TRANSFER_TO_HOST_2D
  -> RESOURCE_FLUSH
```

对一次 scene 变化，旧 visual bounds 为 `B0`，新 visual bounds 为 `B1`，提交区域 `D`
必须满足：

- `D` 覆盖 `B0 ∪ B1`；旧位置和新位置缺一不可；
- 每个 primitive 的实际 paint bounds 都包含在所属 visual bounds 内；若允许 overflow，overflow
  必须显式扩大 visual bounds，或被父节点 clip；
- CPU renderer 从当前 scene 完整重绘 `D`，不能依赖 host 仍保存某个对象的旧像素；
- raster、DRM clip、VirtIO transfer 与 flush 使用同一坐标系、pitch 和区域；
- host resource 第一次驻留时没有历史内容，第一次局部提交必须提升为全帧同步。

最终不变量是：一次提交完成后，scanout 在 `D` 内的每个像素都与当前 scene 一致。单个采样点
正确不能证明该不变量。

## 2. 症状提供的定位信息

| 现象 | 优先检查 |
|---|---|
| 残影长期存在，但被窗口再次覆盖后立即消失 | old visual bounds 漏入 damage，或 child paint overflow 超出 owner bounds |
| 切换 framebuffer 后未触碰区域为黑色/旧内容 | 新 host resource 只做了局部首次同步，或跨 buffer 历史不一致 |
| 损坏按扫描行、固定跨度或 pitch 对齐重复 | clip 到 backing offset 的换算、pitch、SG backing |
| 内容正确但延迟出现，随后自行恢复 | fence、flush、completion publication 或 host frontend 调度 |
| cursor 正确而窗口移动错误 | cursor damage 与 window geometry damage 的生成路径差异 |

这些现象是高信息量的定位线索，不是结论。每个假设都必须由下一层证据证实或证伪。

## 3. 固定诊断顺序

1. **证明运行产物身份。** 分别提取持久 `fs.img` 与可复现 `target/rootfs.img` 内的目标 ELF，
   比较内容摘要。只比较镜像整体摘要会混入 filesystem metadata，不能证明运行的是新 binary。
2. **检查 scene 几何。** 对 mutation 记录 old/new logical bounds、visual bounds、最终 damage union；
   递归确认每个 child 的 paint bounds 是否落在父节点 content/clip 内。
3. **检查 CPU framebuffer。** 在提交前对完整 damage 或固定 tiles 做摘要/差异检查。不要用三个像素
   推断整块区域正确，也不要先假设错误发生在 GPU。
4. **检查 DRM ABI。** 逐项比较半开 clip、framebuffer identity、pitch、inflight snapshot 与失败重并；
   确认没有在 worker 阻塞期间丢掉新 damage。
5. **检查 VirtIO transfer。** XRGB8888 backing 起点为
   `resource_offset + y * pitch + x * 4`，逐行 source offset 与 host destination 必须同步递增；
   首次 resource residency 必须全帧同步。
6. **最后检查 host flush。** 只有前五层均有证据正确时，才增加 QEMU transfer/flush trace。
   热路径临时日志在结论成立后必须删除。

这个顺序先检查最便宜、最接近业务语义的层，避免被“看起来像显存损坏”的画面直接带到最底层。

## 4. 本次残影案例

### 4.1 现象

拖动 recovery 窗口会留下灰色水平块；残影不会自行消失，但窗口随后覆盖相同区域时会被清理。
这已经表明 scanout 仍保存旧像素，而这些像素没有进入后续重绘区域。

### 4.2 被证伪的假设

最初怀疑两只 framebuffer 在局部 page flip 下保存了不同历史。compositor 收敛为一只持久
scanout buffer 后，残影仍可出现，因此该假设不是最终根因。单 buffer 仍是正确的架构收敛：它消除
跨 host resource 的局部历史同步，并节省一份全屏常驻内存，但不能替代 scene damage 的正确性。

### 4.3 根因

`CONTENT_NODE` 相对窗口从 `TITLE_HEIGHT + 1` 开始，高度是
`window.height - TITLE_HEIGHT - 4`；它的 child `SIDEBAR_NODE` 却错误使用完整
`window.height`。在 `TITLE_HEIGHT = 30`、shadow damage 外扩 `8` 像素时：

- sidebar 比 parent content 向下 overflow `34` 像素；
- sidebar paint bottom 比 `window + shadow` damage bottom 多 `23` 像素；
- 每次拖动都把这 `23` 像素画到新位置，却没有把旧位置加入 damage。

修复是计算一次 `content_height`，同时作为 content 与 sidebar 的高度。child paint bounds 重新落入
window visual bounds 后，旧/新 window damage 可以完整恢复背景。

### 4.4 同期消除的独立风险

- compositor 使用一只持久 scanout buffer，geometry 与 pointer 共用同一
  `raster -> DIRTYFB -> TRANSFER -> FLUSH` 路径；
- VirtIO-GPU 对新驻留 host resource 的首次局部 `DIRTYFB` 提升为全帧 transfer，防止未初始化
  区域在未来 framebuffer 使用者中显示为黑色或旧内容。

两项改进强化了显示一致性，但文档和代码评审不得把它们误称为这次 gray trail 的最终根因。

## 5. 长期围栏与验收

- layout node 默认不得 paint 到父节点 content bounds 外；未来开放 CSS `overflow: visible` 前，
  必须先建立独立 visual-bounds/clip owner，并让 damage 消费该 owner；
- window mutation 必须对 old/new **visual bounds** 求 union；logical window rectangle 不能代替
  shadow、border 或显式 overflow 后的真实 paint bounds；
- damage accumulator 容量耗尽只能保守合并为更大的 union，不能丢弃区域；
- presenter 失败必须把完整 inflight snapshot 合并回 pending damage；
- 运行时验收至少覆盖水平、垂直、对角快速拖动，窗口跨越既有残影位置，resize 后继续拖动，
  以及 pointer 与 window geometry 同时变化；任何只能靠后续覆盖才能消失的像素都视为失败；
- 诊断日志必须是有假设、有字段、有删除条件的临时工具，禁止让逐帧串口输出进入正常路径。
