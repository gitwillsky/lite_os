# Wayland 与经典控件验证

## 入口与隔离

`make verify-runtime-wayland` 由 `scripts/verify_wayland.py` 编排，在同一次 AArch64/HVF QEMU 启动中串行验证第一轮的三个独立客户端阶段，以及第二次 cold boot 的会话委派阶段。
所有探针、上游消费者、字体和脚本仅注入 disposable 镜像，产品 rootfs 不安装它们。固定上游版本与摘要见标准基线。

## 服务端与外壳协议

同一 QEMU 显示会话使用固定的上游 `wayland-info` 与本仓库的 `wayland-probe`：`wayland-info` 必须列出 `wl_compositor`、`wl_shm`（含格式）、
`xdg_wm_base` 与 `wl_output`（模式、scale、名字）；探针走完 xdg configure/ack、五帧 shm buffer 与 frame callback；`screendump` 里
窗口矩形是最后一帧颜色、其外是桌面背景、位置是层叠放置的第一个窗口；收到的 keymap 由 guest 里链接上游 libxkbcommon 的 `xkb-check`
（禁用默认 include，逐键与修饰键索引裁决）编译；注入平板移动/点击与 Shift+A，客户端必须按序收到 `wl_pointer` 的 enter/motion/button/leave
与 `wl_keyboard` 的 modifiers/key（局部坐标按输出 scale 换算）；随后依次拖动标题栏、拖右下角缩放、最大化/还原、全屏/还原、
最小化与 Alt+Tab 恢复，每步都由截图核对窗口的实际位置与大小；最后注入 Alt+F4，窗口必须消失、桌面恢复。
同一次启动随后用注入 `/bin/logon` 的 `shell-probe` 走真实特权 socketpair：映射背景与任务栏，
通过 foreign handle 最大化/最小化/激活/关闭另一个连接的窗口，验证工作区随 panel 高度变化、OnDemand 点击焦点、Exclusive 键盘阻止 Alt+F4 操作应用，以及外壳连接退出后的图层/焦点清理。
公开 `wayland-info` 不得出现 layer-shell 或 security-context manager；foreign-toplevel 向普通桌面应用开放，任务管理器依赖该协议。
硬件光标平面不进入 screendump，光标路径以 guest 实际 DRM 更新成功裁决，不宣称截图验证了 cursor plane。
新增外壳布局与列表 codec 不引入逐像素路径、锁或共享状态：每次事件循环投影窗口/图层快照，foreign 匹配最坏为 O(manager 数 × 窗口数²)，
未变化的 foreign 列表零输出由确定性单测裁决；沿用 canvas blocking benchmark，不新增受宿主调度影响的 wall-clock 阈值。协议状态机另有进程内测试（`user/programs/compositor` 的 `cargo test`，含 socket、乱序/拆包/随机输入）。只在 AArch64+HVF 承担。

## 显示性能与帧交接

内核显示/GPU gate（`scripts/verify_gpu.py`）是上述“宿主墙钟不作 blocking gate”原则的唯一显式例外，因为它不测量
宿主墙钟：帧间隔取自 guest 内核单调 vblank 时钟。该时间戳由内核在 deferred display 完成路径以
`get_time_ns()`（DTB timebase 单调计数）写入 DRM page-flip 事件（seconds/microseconds 与 sequence），与 Python
宿主时钟、HVF 调度抖动完全无关。gate 在固定 UTM VirGL 产品实例中启动 rootfs 的私有副本，先以固定上游 `eglinfo -B -p gbm -a gles` 创建 VirGL GLES context，再运行
`drm-flip-probe`（`user/libraries/linux-uapi/src/bin/drm-flip-probe/main.rs`；只安装在私有镜像）。
随后通过真实 display-service 启动 compositor，并运行上游 Weston simple-egl；它会实际创建窗口和共享 GPU 缓冲。
探针走产品同一内核路径：VirGL2 能力、render target、EXECBUFFER 清屏、PAGE_FLIP。它先对清屏结果做像素读回比对
（功能裁决），再连续翻页，丢弃 60 帧 warmup 后测量 600 次完成间隔。`dropped` 取设备 vblank sequence 间隙，严格要求
为 0；p95/p99 按“宽但真实的绝对上限”设阈值，不在文档记录本机测量值。样本不足、读回像素不符、flip 事件与提交帧
不对应都使 gate 变红。此 gate 只在 AArch64+HVF 承担；RISC-V TCG 没有 VirGL 显示路径，脚本在该目标自跳过。
统计与判定逻辑由探针内的 host 单测和 `scripts/tests/test_verify_gpu.py` 约束。

显示服务门禁（`scripts/verify_display.py`，`make verify-runtime-display`）在 QEMU（virtio-gpu、键盘、平板，无窗口）冷启动产品 rootfs：
先等 compositor 的首帧与输入就绪标记，再经 QMP `screendump` 取回 scanout，与 `LBS1` 参考解码器按同一整数缩放居中规则算出的整帧逐像素
比对；内核 `boot_screen` 与 compositor 任一方的缩放、居中或进度填充规则漂移都会失败。随后注入一次平板移动与一次按键，要求
compositor 报告各自的首个事件。它不测量时间，参考帧与比对函数由 `scripts/tests/test_verify_display.py` 约束；只在 AArch64+HVF 承担。

## classic-ui 消费者

第三阶段 `ui-probe` 通过 `classic-ui` 的公开 API 建立 xdg 窗口，加载同一份主题和字体，画出中文标题、编辑框与默认按钮。
门禁检查客户区面色、编辑框窗口色与窗口外背景，并注入以下输入：

1. 输入文本，Ctrl+A 替换；Tab 进入密码框，只报告字符数对应的掩码；Shift+Tab 返回前一个控件。
2. 按住字符键，等待由客户端 repeat_info 时钟产生的重复；释放后用新输入替换，并用 Enter 触发默认操作。
3. 拖动真实标题栏、拖动右下边框；等待新几何的帧上屏后点击最大化、还原、关闭按钮。
   每次交互先确认协议事件，再检查相应像素；configure 本身不能代表新尺寸已上屏。

这里验证经典控件库的消费路径，不是登录认证，也不把两个编辑框称为完整 logon。
`classic-ui` 模块测试另覆盖 configure/ack 原子性、重复键取消、对象编号回收、UTF-8 编辑边界和按钮按下/释放配对。
1x/2x 的绘制仍由像素测试与 gallery 覆盖；本门禁实际 guest 输出为 scale 1，不冒充 HiDPI 端到端验收。

## 客户端成本决策

客户端每个窗口只允许一个未完成 frame callback；应用在 ready_to_draw 为真时才渲染，避免积压整帧 CPU 工作。
每个窗口最多 3 块 memfd 缓冲轮换；`Display::present` 与上一次提交逐行比较，只经 `MemFd::write_segments` 写入变化区域
（宽矩形连同行间一次写，窄矩形逐行写）并以 `damage_buffer` 声明，复用缓冲补写它落后的各帧损伤。比较是一次 O(帧像素) 的内存比较，
换掉的是整帧共享内存写入、合成器整帧 `pread` 与纹理整幅上传。
buffer.release 回收资源，断开由 RAII 清理。
合成器只读取损伤矩形：宽矩形按 64 KiB 预算批量 `pread`（大 stride 时至少一行），窄矩形逐行读，保留截断文件的协议错误语义。
`canvas::Surface::blit` 对同倍率且实际像素完全不透明的可见行直接复制；其余保留预乘 alpha 合成与整数缩放。
该快路径不缓存不透明状态，也不信任客户端声明。O(帧像素数) 的成本由相同工作负载的离线前后微基准与既有 canvas 基线评估；
SHM 的 stride/offset/末行/truncation、blit 语义测试与真实窗口的重复提交、尺寸变更验证等价性。
不设宿主墙钟阈值；复用 shm 或添加动画时需以目标 workload 再作测量。
编辑按键不复制整份文本；UTF-8 查找和 String 编辑为单行长度的 O(n)，绘制时仅密码掩码生成临时文本。

## 会话委派与监听状态

第二次 cold boot 用 session-probe 替换私有镜像中的 logon：初始 root 客户端绘制 Exclusive Overlay，
在探针触发键之后注册标准 security context，并先断开父 Wayland 连接，再通过标准 Command 拉起 uid/gid 65534 的子进程。
子进程从 /proc/self/status 核对 real/effective/saved/fs 身份均已改变且补充组为空；继承连接仍能创建背景与 panel，但不能再委派 context。
随后由真实按键驱动 panel 高度变化、unmap、重新 configure/ack 与 remap；退出后屏幕恢复主题背景。
这里没有密码认证，子进程身份也不是通过伪造 metadata 得到的。

listener-check 使用固定 musl 头文件和 Linux getsockopt，覆盖 UNIX/IPv4 TCP 的新建、监听、accepted child 和 UDP/socketpair 的 SO_ACCEPTCONN 值。
macOS 没有相同 SO_ACCEPTCONN getter，宿主 metadata/所有权测试从 std 创建的 typed listener 开始；原始 fd 验证和 SCM_RIGHTS 在 Linux guest 中真实执行。
针对性宿主测试另外裁决：metadata 不可重复设置、commit 不可重复、创建者断开后 listener 独立存活、同轮 HUP 优先于 accept、
撤销不终止既有连接，以及永久 write 错误不会留下反复刷新的 closing 连接；HUP 优先级用语义快照裁决，guest 另要求真实 HUP 产生撤销标记。

新增 context 只在会话建立/撤销时分配 fd、metadata 和 listener 表项；每个 listener 在现有 poll 集合增加两个条目，
不经过每像素、每字节或每次普通窗口 commit 的热路径。SO_ACCEPTCONN 只读取已有 owner guard，因而不新增 wall-clock benchmark 阈值；
既有 benchmark 和实际 guest 会话工作负载承担成本回归，未声称这是完整登录或进程沙箱的验收。

产品 `logon` 退出会让 compositor 交由 init 重建。机制探针在协议 owner drop/shutdown 后保持监督进程存活，
因此仍验证真实 EOF 清理、创建者断开后的委派与普通 UI 客户端，不依赖探针退出绕过产品监督策略。
完整的账户、桌面与音量 UI 由 [产品桌面门禁](desktop-verification.md) 裁决。

## 提交、共享缓冲与验证范围

| 范围 | 实际裁决入口 | 证明边界 |
|---|---|---|
| surface 状态机 | compositor host 测试 | 初始 configure、显式 null attach、sync/desync 子树、输入 region、在途 callback 与角色错误 |
| CPU 仿射采样 | viewport 的真实 Canvas 测试 | 八种 buffer transform、buffer scale、分数源裁剪；保留同一像素 owner |
| dma-buf 参数 | dmabuf 协议测试 | 重复/缺失平面、非法尺寸、真实 FD 越界、可表示但平台不支持的布局，错误与 failed 路由不同 |
| 内核共享寿命 | GPU 私有镜像探针 | self/repeated import、ARGB/XRGB 同 handle 视图、生产者 context/OFD 和全部 dma-buf FD 关闭后消费者仍能等待隐式 fence、读回正确像素 |
| GEM VMA lease | mapping_retirement host 测试 | 映射保活设备对象与实际 OFD；最后 descendant 释放，退休路径保持先撤销、TLB fence、再析构 |
| 不可变 feedback | memory-file guest fixture 与 host seals 测试 | WRITE/FUTURE_WRITE、EBUSY 原子失败、新旧共享写映射、mprotect、拆分/fork、私有 COW |
| 第三方 EGL | GPU 私有镜像中的未修改 Mesa/Weston | GBM GLES renderer 必须为 VirGL；Wayland 不透明和 alpha 两种窗口均实际 create_immed，至少三次 buffer.release，正常客户端退出 |
| 显示节拍 | GPU 探针既有 600 帧 gate | 像素读回、exact flip sequence、零 dropped，沿用原 p95/p99 阈值 |
| SHM 与桌面交互 | 本页 Wayland gate、[桌面 gate](desktop-verification.md) | 真实冷启动、截图与输入；不以 EGL 路径通过替代 SHM 回归 |

GPU 串口完整记录在 `target/logs/gpu-gate-serial.log`，构建和门禁输入由内容指纹绑定；
出现 Mesa rejected submission 或 kernel panic 立即失败。UTM 产品实例必须已停止，门禁结束恢复原配置，修改仅发生在私有镜像。

CPU 输出和 GPU 合成共用标准提交元数据；CPU 八种方向的像素结果不冒充 GPU 每种方向的端到端截图。
硬件光标不进入 scanout 截图，光标热点/动画仍需单独显示观察；1x 的 guest 交互不代表 2x 的整套 UI 验收。
原生控件仍使用 wl_shm；当前 dma-buf 平台只支持同设备、单平面 ARGB/XRGB、offset=0、紧密 stride、隐式 modifier。
不支持的有效布局按标准导入失败处理，不能伪装成 FD 越界，也不公布未实现的 LINEAR/scanout tranche。
完整 OpenGL/Vulkan、跨设备 PRIME、blob 与 sync_file/syncobj 不在通过声明内。

性能基线仍运行 canvas 与既有 GPU gate。尚未分别测得首次上传、已提交窗口拖动、缩放/还原和纹理替换的前后成本，
因此不宣称某个优化提高了多少帧率。SDK 的 guest 内编译、旧 ELF 升级和源码调试由 [SDK 开发](ui-sdk.md) 独立记录。
