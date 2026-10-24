# Wayland 与经典控件验证

## 入口与隔离

`make verify-runtime-wayland` 由 `scripts/verify_wayland.py` 编排，在同一次 AArch64/HVF QEMU 启动中串行验证第一轮的三个独立客户端阶段，以及第二次 cold boot 的会话委派阶段。
所有探针、上游消费者、字体和脚本仅注入 disposable 镜像，产品 rootfs 不安装它们。固定上游版本与摘要见标准基线。

## 服务端与外壳协议

Wayland 门禁（`scripts/verify_wayland.py`，`make verify-runtime-wayland`）在同一 QEMU 显示会话里注入固定的上游 `wayland-info` 与本仓库的 `wayland-probe`（私有镜像，产品 rootfs 不含）：`wayland-info` 必须列出 `wl_compositor`、`wl_shm`（含格式）、
`xdg_wm_base` 与 `wl_output`（模式、scale、名字）；探针走完 xdg configure/ack、五帧 shm buffer 与 frame callback；`screendump` 里
窗口矩形是最后一帧颜色、其外是桌面背景、位置是层叠放置的第一个窗口；收到的 keymap 由 guest 里链接上游 libxkbcommon 的 `xkb-check`
（禁用默认 include，逐键与修饰键索引裁决）编译；注入平板移动/点击与 Shift+A，客户端必须按序收到 `wl_pointer` 的 enter/motion/button/leave
与 `wl_keyboard` 的 modifiers/key（局部坐标按输出 scale 换算）；随后依次拖动标题栏、拖右下角缩放、最大化/还原、全屏/还原、
最小化与 Alt+Tab 恢复，每步都由截图核对窗口的实际位置与大小；最后注入 Alt+F4，窗口必须消失、桌面恢复。
同一次启动随后用注入 `/bin/logon` 的 `shell-probe` 走真实特权 socketpair：映射背景与任务栏，
通过 foreign handle 最大化/最小化/激活/关闭另一个连接的窗口，验证工作区随 panel 高度变化、OnDemand 点击焦点、Exclusive 键盘阻止 Alt+F4 操作应用，以及外壳连接退出后的图层/焦点清理。公开 `wayland-info` 输出出现任一外壳全局即失败。
硬件光标平面不进入 screendump，光标路径以 guest 实际 DRM 更新成功裁决，不宣称截图验证了 cursor plane。
新增外壳布局与列表 codec 不引入逐像素路径、锁或共享状态：每次事件循环投影窗口/图层快照，foreign 匹配最坏为 O(manager 数 × 窗口数²)，
未变化的 foreign 列表零输出由确定性单测裁决；沿用 canvas blocking benchmark，不新增受宿主调度影响的 wall-clock 阈值。协议状态机另有进程内测试（`user/compositor` 的 `cargo test`，含 socket、乱序/拆包/随机输入）。只在 AArch64+HVF 承担。

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
首版每帧创建一个不可变 memfd，像素转换使用固定 4096 字节 scratch，额外内存不随整帧字节数再复制一份。
buffer.release 回收资源，断开由 RAII 清理，不增加线程、锁、Atomic、全局 cache 或 raw FFI。
该路径的成本为 O(帧像素数)，当前由真实窗口的重复提交、尺寸变更和已有 canvas blocking benchmark 裁决正确性与光栅基线，
不新增宿主墙钟阈值；它尚不是高帧率动画的性能承诺。后续复用 shm 或添加动画时需以目标 workload 再作测量。
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
