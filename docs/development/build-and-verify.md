# 构建、测试与验证

本文件唯一维护开发环境、构建入口、缓存规则和质量门禁。文档只定义可重复的输入、覆盖与成功条件，不记录某次执行日期、输出或实测数值。

## 环境

- Apple Silicon macOS 先安装 Xcode Command Line Tools（`xcode-select --install`）与 Homebrew，
  再执行 `make setup`。`scripts/host_setup.py` 是 host 工具安装的唯一 owner：用 Homebrew 安装
  `llvm`（Clang、`llvm-ar`、`llvm-ranlib`、`llvm-readelf`）、`e2fsprogs`、`qemu`、
  `openssl@3`；安装 rustup 并按
  `rust-toolchain.toml` 安装固定 toolchain；按固定 SHA-256 安装 UTM v4.7.5；各步骤幂等；已存在非固定版本 UTM 时 fail-stop，不覆盖用户应用。缺少 `llvm` 会让
  musl 构建以 `Clang, LLVM archive tools, and the pinned Rust rust-lld are required` 失败。
- Rust 版本、组件和 target 由 `rust-toolchain.toml` 固定；精确 revision 见 [规范基线](../standards-baseline.md)。
  kernel ISA baseline 由 target 定义，不使用 host `target-cpu=native`；跨 ISA 编译时 host CPU
  不属于目标 LLVM backend，native 会造成无效 CPU/codegen，且让同一源码随构建宿主改变指令。
- `ARCH` 默认 `aarch64`，只接受 `aarch64` 与 `riscv64`；它统一选择 kernel target、Linux userspace target、QEMU、musl loader 与 Alpine repository architecture，未知值在 Make 解析期失败。
- `ACCEL` 默认 `hvf`，只接受 `hvf` 与 `tcg`。`riscv64 + hvf` 在构建前硬失败；RISC-V 必须显式选择 `ACCEL=tcg`，AArch64 的 TCG 诊断路径也必须显式选择，不能从 HVF 静默回退。
- `PROFILE` 默认 `release`，只接受 `release` 与 `debug`；提交门禁以 release 产物为准。
- `QEMU_MEMORY` 默认 `2G`，统一控制 `run`、`run-gui` 与 `run-gdb` 的 guest RAM；交互式
  AArch64 runtime gate 使用相同的 2 GiB 产品内存配置。
- `AGENT_QEMU_MEMORY` 默认 `6G`，只控制 `prepare-agent-development` 的原生 CLI smoke 与
  `run-agent-development`；不能用 Agent 的重型内存需求放宽产品 2 GiB runtime contract。
- musl、BusyBox、APK 和 terminal font 输入必须由脚本中的固定 URL、版本与摘要构建；musl、BusyBox 与 APK 都为所选架构生成或下载原生产物，不得静默消费另一架构 cache、系统副本或滚动 latest。
- kernel 位于 `target/<kernel-target>/<profile>/kernel`；可重复 rootfs 基线位于 `target/rootfs/<arch>.img`，开发实例是 `fs-<arch>.img`，只由显式 reset 初始化且不能反向污染基线。musl、BusyBox、APK 与 runtime success cache 均带 architecture
  identity，不允许跨目标命中。
- rootfs 基线把同一个 BusyBox `env` inode 发布到 `/bin/env` 与 `/usr/bin/env` 并验证权限/identity，
  保证 community 中 npm/pnpm 等 `#!/usr/bin/env node` 入口安装后可执行。
- `FS_IMAGE_SIZE_MIB` 只控制可写开发实例的最小容量，默认 8192 MiB；`run`、`run-gui` 与
  `run-gdb` 在 QEMU 启动前离线扩容已有实例并保留内容，较大的实例不会被缩容。缺少该扩容会让
  基线派生的 512 MiB 实例在安装 Node.js 等应用时以 `ENOSPC` 失败；runtime gate 仍消费固定、
  可复现的只读基线，不继承开发容量。
- `AGENT_FS_IMAGE_SIZE_MIB` 默认 32768 MiB，只由 Agent 开发入口消费。固定 Node/npm、
  Bash/ripgrep 与 Git/curl APK closure 先离线安装到 `fs-aarch64.img`，Guest npm 再从 host
  按 registry SRI 校验并固定的 cache 全局安装 Codex 与 Claude；npm package/platform
  identity、APK metadata、package database、唯一 `/usr/local/bin` 入口与真实 `--version`
  执行共同决定成功。产品 rootfs 不包含 Agent、Node/npm 或滚动 repository。
- `sync-userland` 离线同步 UART 基线的 init、rcS、profile、shutdown 与共享启动素材，并移除旧镜像中退休的
  图形 binary、私有会话脚本和呈现素材；不修改 `/root`、用户安装的 APK 或项目。镜像内内容指纹命中时 no-op。
  `run` 与 `run-gui` 自动同步；`reset-rootfs` 才从基线显式重建开发镜像。
- Rust 验证探针以 target、工具链、sysroot、libunwind 与 link flags 隔离 Cargo compilation cache，
  最终 ELF 按完整源码内容发布不可变 generation。
- `run-gui` 当前只显示 kernel 启动画面，交互 shell 位于 UART；没有显示器文本控制台、桌面或 seat 切换。
- macOS 的 `run-gui` 是唯一交互图形产品入口：要求固定 `/Applications/UTM.app` v4.7.5，生成确定
  UUID 的 UTM v4 配置，通过 UTM public scripting API import 注册，再由同一 API 创建 Metal
  display window。workflow 在 QEMU 启动前预监听 serial TCP，并把该次启动的完整 guest 输出覆盖写入
  `target/logs/run-gui-serial.log`；该文件是桌面退出、hang 或窗口关闭后的持久取证 owner。
  Make/Python 前台进程持有生命周期；Ctrl-C 只停止该 managed VM。旧 Cocoa QEMU window、激活脚本
  与 GUI 参数入口均不存在。
- UTM 产品路径固定 AArch64 `host` CPU、HVF、SPICE `gl=on`、`virtio-gpu-gl-device`、native Retina
  dynamic resolution，以及 UTM 自带的唯一 PCI `com.redhat.spice.0` port。kernel/rootfs 以同卷
  hard link 发布到 UTM app group，QEMU drive 显式 `format=raw`。产品 graphics boot 与 gpu gate
  都创建同一 UTM/VirGL 配置的 disposable 实例：gate 先备份开发 VM 的 plist 与已发布 artifact
  inode，使用预监听 TCP client serial 捕获最早 boot marker，
  结束后精确恢复原配置与 inode。`run`/`run-gdb` 只保留无窗口 QEMU/MMIO 拓扑作为非图形验证与调试
  入口，不是第二个 GUI 产品实现；两条设备 transport 在同一个
  transport-neutral VirtIO Console adapter 汇合。
- userspace compiler owner 是含目标 backend 的 Clang driver、固定 Rust toolchain 的 `rust-lld` 与
  固定 bare-metal `compiler_builtins`：AArch64 为 hard-float AAPCS64 `aarch64-unknown-none`（kernel
  独立使用 `aarch64-unknown-none-softfloat`，两者不得混用），RISC-V 为 lp64d medany
  `riscv64gc-unknown-none-elf`。固定 `compiler_builtins` 只以 `extern "Rust"`
  导出 f128 complex helper，`scripts/verify_musl.py` 因此从 rustc 同版本 LLVM compiler-rt 逐文件
  校验 `multc3.c` 闭包，编译 C ABI `__multc3` 并与 rlib 合并为唯一 `libcompiler-runtime.a`；
  toolchain 自身再次导出该符号时构建 fail-stop，要求删除补充。任一 runtime 缺失或歧义都必须在
  发布 sysroot 前失败，musl smoke 必须实际验证 `strtod` 返回、FP arithmetic 与
  `long double _Complex` Annex G 重算。Homebrew 裸机 `libgcc` 是 medlow 非 PIC 代码，不能进入
  `libc.so`，因此不作为任一架构的 userspace runtime。
- 标准 Rust userspace 由单一 `user/` Cargo workspace/lockfile 构建；产品应用与
  `scripts/verify_rust_std.py` fixture 都使用固定 rust-src。Cargo 直接生成最终 binary，禁用 bundled
  musl CRT，动态链接项目 musl，并静态链接同 revision LLVM libunwind；禁止 staticlib 后手工二次链接。libunwind 是 panic/backtrace
  冷路径且固定 `-O2`；其正确性由双架构 target runtime gate 裁决，不增加失真的 host wall-clock benchmark。
- BusyBox/OpenSSL 共用 verify_musl 的 `llvm-strip` resolver，从 PATH、Clang 相邻工具及 Homebrew 位置解析；
  cache fingerprint 记录实际执行文件的绝对路径与内容摘要，构建直接消费该已选路径。
- musl PID1 fixture 在 fork consumer 前创建 `/tmp` 并挂载 proc、sysfs、devpts，继承同一初始环境。
  无效 ancillary 指针与只读 msghdr 的故障顺序直接调用标准 syscall，避免 libc 的预复制或写回先崩溃。
  未屏蔽 SIG_IGN 的 generation 直接使用 rt_sigqueueinfo，避免 musl sigqueue 的临时屏蔽改变条件。
  `CLONE_THREAD` fixture 使用现有 musl 汇编 trampoline，仅执行 syscall，不加入 pthread 列表。
- seccomp regression 直接执行生产 BPF verifier/security owner，覆盖 scratch 路径初始化、
  signed action precedence、newest DATA、全部 seccomp_data words、OOM 不发布与 6554-program
  小栈析构；musl 双架构 1/2 CPU gate 覆盖 unprivileged NNP 授权、fork/thread/exec 继承、
  strict/thread/group fatal、SIGSYS siginfo/原寄存器与 RISC-V 私有 syscall interception，及
  TSYNC 的既存/后续 thread 继承与 divergent TID/ESRCH 零变更失败。host transaction fixture
  执行生产 graph commit 算法，覆盖 stale prepare、容量增长/OOM、退出排除、group exit 与
  NNP 并发置位；graph/IRQ adapter 为宿主实现，不能替代实际 clone publisher/退出阻塞交错。
  TSYNC CPU 诊断在 128..384-program chain 上测量 1 与 64 个 target，含 host graph adapter，
  只用于 CPU 成本判断，不能外推 IRQ latency/设备或 native 产品门禁。
  signal-frame consumer 独立损坏两架构的 extension header，并以不可读 SP 调用 rt_sigreturn；
  父进程验证 SIGSEGV 默认终止，handler 验证 RV SI_KERNEL、AArch SP/code 与未对齐可读 frame，
  排除后续 trap 假阳性；query mask/copyout canary 与自定义 restorer 验证 sigaction 架构布局。
  benchmark 使用生产 owner 测量 disabled/单 filter/八 filter；disabled 不锁 state、不读取 PC，
  interpreter 无分配，forward branches 保证步数不超过 validated path cost。


## 持久本地输入与实验

`scripts/local_inputs.py` 唯一维护工程内 `.local/` 的路径；整个目录被 Git 忽略，
不提交第三方下载、用户原版资源、私钥或实验镜像。版本、URL、摘要和配方仍由对应脚本提交。

| 位置 | 内容与 owner |
|---|---|
| `.local/inputs/musl/<arch>/` | musl/Linux/GNU Make 归档、解包源码和 compiler-rt 固定源；`verify_musl.py` |
| `.local/inputs/busybox/<arch>/` | BusyBox 归档与源树；`verify_busybox.py` |
| `.local/inputs/openssl/` | OpenSSL 源码、归档与固定 CA bundle；`openssl_cache.py` |
| `.local/inputs/alpine/<branch>/<arch>/` | 固定 APK 与上游 bootstrap 解包文件；`apk_cache.py` |
| `.local/inputs/agents/<arch>/` | Agent 离线 npm cache 与 APK 输入；`agent_cli_cache.py` |
| `.local/inputs/fonts/`、`host/`、`windows2000/` | 字体源、host 安装器、用户原版素材与本地覆盖包 |
| `.local/keys/apk/<arch>/` | 本地 repository 签名身份；`apk_cache.py`，迁移时保留原 key pair |
| `.local/tools/` | host Python 虚拟环境；不依赖 target/ 存活 |
| `.local/spikes/<主题>/` | 实验脚本、源码、VM 与基线结果；经典桌面实验位于 `classic-desktop/` |

`target/` 保留编译 cache、sysroot、构建工具产物、验证镜像、临时工作区、日志和 success stamp。
`clean-musl` / `clean-busybox` 或删除 `target/` 只使产物重新构建，不再下载已校验固定输入，
也不重建本地签名身份。实际构建仍逐项检查摘要、manifest 和架构；目录迁移不放宽输入验证。
本地目录不进入架构源码扫描；仓库内有本地修改的 `vendor/smoltcp` 仍属于受版本管理的实现源码。

## 经典桌面本地素材

这是经典桌面设计阶段的宿主准备流程，不进入产品构建、rootfs 指纹或 runtime gate。
素材语义、原版来源及映射由[素材清单](../design/classic-desktop/assets.md#本地原版素材)拥有。
先加载 shell 环境，安装离线提取工具，并准备独立 Python 环境：

```bash
source ~/.zshrc
brew install sevenzip cabextract icoutils
python3 -m venv .local/tools/classic-assets
.local/tools/classic-assets/bin/pip install Pillow==13.0.0
.local/tools/classic-assets/bin/python scripts/extract_windows2000_assets.py \
  --iso "$HOME/Downloads/Windows 2000 Professional Service Pack 4 CHS.iso"
python3 scripts/extract_windows2000_assets.py --check
```

提取器只读取 ISO/PE 数据，不在宿主执行镜像内的程序。默认输出是被 Git 忽略的
`.local/inputs/windows2000/assets/`；`--output` 也只能指向本仓库 `.local/inputs/` 内。
已有目录不覆盖；重新提取使用新的 `--output`，核对后显式替换。
失败会清理临时目录，完整解码与哈希检查通过后才发布最终目录。`--check` 不需要 Pillow 或外部提取工具。
工具原始资源长度警告保存在 manifest 中；不能把完整性检查误报为画面/语义逐项验收。

此流程只增加宿主离线工具，没有新增 kernel/userspace 热路径、锁、分配或 codec，故不新增 benchmark。
验证包括独立提取的 manifest 一致性、全部原生图像帧解码、文件损坏/路径越界/悬空引用测试，
不依赖 Windows 2000 参照机；产品完整门禁仍是 `make verify`。

## 推荐入口

```bash
make setup
make build
make run
make run-gui
make verify-fast
make verify-runtime
make verify
```

`make verify` 是提交前完整入口；`verify-fast` 用于日常反馈，`verify-runtime` 用于单独复现 guest
门禁。所有入口都接受 `ARCH`、`ACCEL`、`PROFILE` 及 QEMU 相关环境变量。

`make verify-runtime-shutdown` 使用 `make run` 的 headless 设备装配，在单 CPU 与宿主默认
SMP topology 冷启动 private rootfs，执行 BusyBox halt/poweroff/reboot，要求完整 shutdown
marker 和 QEMU 正常退出；不允许 headless 图形会话重试刷屏。reboot 用 `-no-reboot` 把
guest reset 转成可裁决的 QEMU 退出，不能用 host 强制终止冒充 reset 成功。

`make diagnose-scheduler-latency` 回答“调度器没有空闲 CPU 主动拉取或推送 Ready 任务，这个缺口有多大”。它不隐式
重建 kernel 或 rootfs，在 HVF 之外的结果不具代表性。它在无窗口 QEMU 里运行一个 guest 程序
（`scripts/fixtures/diagnostics/scheduler-latency.c`），报告两个指标：

- 空闲 CPU 等到排队任务的延迟：一个任务排队在 CPU0 上，CPU1 刚变空闲，直接测量它多久后才出现在 CPU1。
- 突发 CPU 密集任务集的总耗时相对**理想处理器共享**的额外百分比。理想值是零迁移延迟、永远工作守恒的公平调度；
  时间片轮转本身就达不到理论下界，不能与下界直接比较。同一任务集由相同种子在 guest 与宿主两边生成，工具每次运行前
  先让 guest 打印任务长度并与宿主比较，两边漂移时直接失败。逐次配对计算，所以尾部分位数有意义。

额外耗时包含 fork、唤醒、tick 粒度等全部开销，因此是均衡延迟损失的上限。结果受宿主 QEMU 线程调度影响，应重复运行
并关注分布而不是单次数值。

局部 scope 仍可直接调用，但它们不是独立编排入口：

| scope | 用途 |
| --- | --- |
| `build-kernel`、`build-rootfs`、`build-rust-std` | 定位单项构建问题 |
| `sync-userland`、`reset-rootfs` | 准备或恢复开发镜像 |
| `verify-unit`、`verify-architecture-benchmark` | 定位 host/static/unit 问题 |
| `verify-runtime-*`、`verify-runtime-gates` | 定位单个 guest 门禁；由 `verify-runtime` 统一串行调用 |
| `regen-font`、`regen-ui-font`、`regen-icon-font`、`clean*`、`gdb`、`addr2line` | 维护和调试工具 |
| `diagnose-scheduler-latency` | 量化多 CPU 调度的均衡延迟；诊断工具，不判定通过与否，也不属于任何门禁 |

## Agent 开发镜像

`make prepare-agent-development` 只支持一等 AArch64 路径。它先把持久开发实例扩到 32 GiB，
再用 6 GiB Guest RAM 执行固定 APK transaction，并由 Guest npm 从已校验 cache 离线全局安装
固定版本的 Codex/Claude。已有音乐、项目、认证配置和其他用户数据不由该入口管理；内容身份
命中时只回读 npm package identity 与 APK database，不重复安装。

```bash
make prepare-agent-development
make run-agent-development
```

进入 Guest 后先注入密钥，不得把密钥写入仓库或 rootfs：

```sh
printf '%s' "$OPENAI_API_KEY" | codex login --with-api-key
codex login status
codex exec --sandbox danger-full-access '读取当前仓库并运行最小检查'

export ANTHROPIC_API_KEY='...'
claude -p '读取当前仓库并运行最小检查'
```

LiteOS 当前没有 bubblewrap 依赖的 Linux namespace/seccomp/Landlock 完整契约，因此 Agent
开发镜像不宣称 sandbox 隔离。任何 CLI 首次出现的 `ENOSYS`、fatal trap 或 hang 都必须定位
第一个 kernel/userspace 契约缺口并补正式 Linux 语义，禁止增加 CLI 专用假成功分支。

## 单元测试

`verify-fast` 的 unit scope（也可直接执行 `make verify-unit`）必须包含：

- `architecture-check`：dependency、owner、interface、文档索引/链接/事实归属与退化模式的纯函数测试。
- `kernel-unit`：复用 production path 的内存、文件、IPC、socket、codec、数据结构与错误边界测试。
  ext4 cost/recovery 用例只读取 `verify_unit` 每次经 `create_fs.py` 重新生成的
  `target/kernel-unit/ext4-fixture.img`（`LITEOS_EXT4_FIXTURE`），写入只进入内存 overlay；不读取产品
  rootfs 或开发实例，因此 `verify-fast` 无需先构建 rootfs，直接 `cargo test` 会以变量未设置失败。
  ext4 conformance 用例在 fixture 的临时副本上写入，再以 Homebrew e2fsprogs 的 `e2fsck -fn`
  （`LITEOS_E2FSCK`）裁决零错误；PATH 上的其他 mke2fs/e2fsck 不参与解析。
  ext4 写路径门禁在 sync 后裁决三类代表负载的 transaction/flush/设备写入上界，崩溃矩阵在提交的每个
  barrier 后崩溃并以 `e2fsck -fn` 裁决恢复结果。
  platform DTB 解码用例读取 `verify_unit` 以 runtime gate 相同 machine 配置（TCG、2 CPU）经 QEMU
  `dumpdtb` 现场生成的 `target/kernel-unit/qemu-virt-{aarch64,riscv64}.dtb`（`LITEOS_<ARCH>_DTB`）。
- `scheduler-unit`：preallocated ready heap 的 capacity/compaction/fail-stop 与 signal selection/generation 测试。
- `user/` Cargo workspace：linux-uapi、keyboard、terminal-model、terminal-font、canvas、classic-theme、classic-ui、typeface、wayland-scanner 与 wayland-wire 的单元测试。
- run membership/CPU projection 由 `architecture-check` 静态围栏，wait/deferred 边界由 `kernel-unit`，完整 lifecycle 由对应 runtime smoke 覆盖；不得把这些范围虚报为 `scheduler-unit` 用例。
- filesystem 可睡眠 owner 由静态字段/adapter contract 与真实 `TaskMutex` FIFO handoff host
  test 共同约束：两个排队 waiter 必须按 ticket 获取，handoff 期间 `try_lock` 不得插队，
  completion-before-arm/arming/sleeping 三种顺序均不得丢 wake。
- `syscall-abi`：编号唯一性与 ABI 表一致性测试。

kernel-unit 的 ASID 用例直接执行生产 generation owner，用 host local-TLB adapter 保留旧
root canary，覆盖 8/64/128/256/65536 容量的零号保留/耗尽换代、并发切换、准备 OOM、
退休拒绝、跨多代 idle CPU、同 mm 多 reservation 续代与 CAS 后迟到 seen 的确定交错。
local IRQ/fence adapter 不证明硬件 TLB；musl 在各架构 1/2CPU 建立 272 个同时 live 且
实际 activated 的 private root，在相同 VA 保留不同 canary 并交错 yield。AArch64 TCG
另以标准 cortex-a72 CPU、128MiB RAM 在 1/2CPU 冷启动同一 kernel，要求实际 probe
capacity=256 的 boot marker，272 live roots 因而覆盖硬件换代；默认 max/host 的 16-bit
结果不冒充 8-bit 换代证据。RISC-V 16-bit 的换代边界由同一生产算法 host 回归覆盖。
热路径另测 token clone/activate/drop 的固定 warmup 与五次 median，不设置宿主阈值。
registry 用例在独立 child allocator 中重入生产 reclaim，并用 watchdog
裁决自锁，验证两个 registry 的 OOM 不发布与 dead slot 无分配复用。退出 token 用例
交错 Process mm replacement，验证旧 backing pin、clear-tid 和 futex key 不触及新映像。
reboot/prctl 用例直接执行生产 syscall codec，验证权限优先级、严格参数和单向状态。
这些均为冷路径行为，不添加宿主时序性能阈值。

musl smoke 在 1 CPU 和 2 CPU 各冷启动一次。PID1 fork/wait 实际 consumer，只有
真实 exit=0 才发布最终 marker，再调用标准 poweroff；不允许 PID1 return 成功后 panic
依赖宿主 UART chunk 时序蒙混通过。normal/swap 均要求该最终 marker。安全 fixture 通过标准 prctl/clone/fork/exec
检查 no_new_privs 的线程范围、继承和 set-ID 提权抑制，并验证非 root reboot 不进入 reset。
exit/exec fixture 的 worker 将 clear-child-tid 指向 ET_EXEC 的非零初始化全局变量，直接
SYS_exit，主线程不 join 而立即重试 exec；新映像必须保持该变量不被旧退出清零。
host token 回归不能代替该双架构 SMP runtime 验证。

新增行为先补正常、边界、错误、回滚和并发状态转换用例；重构必须保持同一契约。测试不得复制 production algorithm 来制造自洽结果。

## 性能测试

稳定热路径使用 blocking microbenchmark：

- release 构建，固定迭代数，先 warmup；
- 每个样本使用 `black_box`，取奇数样本的 median；
- 只设置宽但真实的绝对上限，用于阻止锁、分配、runtime dispatch 或复杂度退化；
- 文档不记录本机测量值；阈值变化必须有实现和环境证据，不能为通过门禁直接放宽。

当前 blocking benchmark 覆盖 timer deadline、AArch64 VA39 index/TLBI operand projection 与
AArch64 semantic PTE encode/decode、production HAL MMIO window 的检查后 read/write、
seccomp disabled/单 filter/八 filter，以及实际 bdev claim owner 的 raw-read cache policy projection
（一个整盘、一个整盘加八个分区，沿用每项 200 ns/op 上限）。claim gate 只衡量
无竞争 tree/lock CPU 成本，不声称跨 CPU 竞争、alias eviction 或设备 I/O 延迟。HAL 用
对齐的 host buffer 隔离边界/对齐检查的 CPU 成本，不宣称设备延迟或 IRQ 时序。target-specific 零成本边界还必须通过 release target
build、static architecture fence、symbol 与 disassembly 检查；host wall-clock 不能冒充
target instruction cost。RISC-V 保留 backend 的 PTE 与 trap 性能约束由其 unit、release
static gate 与 disassembly gate 继续负责。

新增 hot path、lock、allocation、codec 或 indirection 时必须明确：加入 blocking benchmark、加入 target static/disassembly gate，或说明为何只需要 diagnostic measurement。
whole-machine latency、boot time 与网络吞吐受宿主抖动影响，只作诊断，不作窄阈值 blocking gate。

桌面用户态库的热路径使用各 crate 的 `cargo bench -p canvas -p typeface` 诊断，不设时间阈值：整屏填充约 0.3 ms，
标题栏渐变约 3 ns/像素，24 像素字形蒙版约 2 µs，1x 字体加载约 30 ms（BDF 解析）。结论是 CPU 光栅在目标分辨率下不是瓶颈，
暂不引入 SIMD、字形缓存或脏矩形以外的间接层；compositor 实现后以整帧 workload 再次裁决。

音频逐流增益使用 `cargo run --release -p architecture-bench --bin audio-mixer-bench` 诊断生产
Accumulator 的单路/八路 period CPU 成本；host 硬件浮点不代表 kernel softfloat，故不设置 host 时间阈值。
steady period 不分配、不查 control registry；新增乘法的真实可持续性由 AArch64/HVF 八路、六秒、
零 XRUN 的 workload 和设备 WAV 分析裁决，不声称长时间或 Wine 游戏延迟已经达标。
swap 的 area/slot CPU 成本以生产模块的 release host fixture 单独诊断：
`cargo test -p kernel-unit --release --test fork_file_eof production_swap_slot_cpu_cost -- --ignored --nocapture`。
固定 1/32 area、100,000 次预热和五个样本中位数，测 reserve/retire、clone/retire 与 statistics；
不代表设备 I/O 或竞争，不沿用 syscall 整数投影的纳秒门槛。32 area publication 上限和有界 scan
由生产结构与功能回归约束。canonical resident pin 与 weak page registry 的成本使用
`cargo test -p kernel-unit --release --test fork_file_eof canonical_page_pin_and_bounded_registry_cpu_cost -- --ignored --nocapture`
单独诊断：pin固定10万次预热、五次中位数；registry保留512/4096活页，每批512个新owner，
物理输入在计时前准备。host TaskMutex/物理分配 seam 不代表 native 调度或设备 I/O；
每次最多八个 tombstone probes 的结构界限与故障回归约束复杂度，不放宽时序门槛。
System proc projection 的 resident 遍历从全部页降为零，
通过 scope guard 保证；不引入并行 RSS cache，guest 压力回归验证统计与回收行为。
StorageBudget 新锁只用于页/inode 创建、最后释放及 remount/statfs，不进入已有内容的 read/write；
critical section 仅固定整数运算，无分配/回调。生产 quota 的同类 release fixture 单独测
`production_storage_quota_cpu_cost`（`--test storage_budget --ignored --nocapture`），其 host IRQ
adapter 只代表无竞争计数成本，不宣称实际 local IRQ 指令/设备延迟，不新增虚假的窄时序门槛。
硬件光标静态门禁固定 cursor queue 与 latest-position 合并；当前没有用户态窗口移动路径，
窗口交互性能验收随新 Wayland 会话实现。内核显示/GPU 门禁继续独立保留。

内核显示/GPU gate（`scripts/verify_gpu.py`）是上述“宿主墙钟不作 blocking gate”原则的唯一显式例外，因为它不测量
宿主墙钟：帧间隔取自 guest 内核单调 vblank 时钟。该时间戳由内核在 deferred display 完成路径以
`get_time_ns()`（DTB timebase 单调计数）写入 DRM page-flip 事件（seconds/microseconds 与 sequence），与 Python
宿主时钟、HVF 调度抖动完全无关。gate 在固定 UTM VirGL 产品实例中启动 rootfs 的私有副本，其 `/etc/inittab` 只运行
`drm-flip-probe`（`user/linux-uapi/src/bin/drm-flip-probe.rs`；不进入产品 rootfs，不启动 compositor 与任何 UI）。
探针走产品同一内核路径：VirGL2 能力、render target、EXECBUFFER 清屏、PAGE_FLIP。它先对清屏结果做像素读回比对
（功能裁决），再连续翻页，丢弃 60 帧 warmup 后测量 600 次完成间隔。`dropped` 取设备 vblank sequence 间隙，严格要求
为 0；p95/p99 按“宽但真实的绝对上限”设阈值，不在文档记录本机测量值。样本不足、读回像素不符、flip 事件与提交帧
不对应都使 gate 变红。此 gate 只在 AArch64+HVF 承担；RISC-V TCG 没有 VirGL 显示路径，脚本在该目标自跳过。
统计与判定逻辑由探针内的 host 单测和 `scripts/tests/test_verify_gpu.py` 约束。

显示服务门禁（`scripts/verify_display.py`，`make verify-runtime-display`）在 QEMU（virtio-gpu、键盘、平板，无窗口）冷启动产品 rootfs：
先等 compositor 的首帧与输入就绪标记，再经 QMP `screendump` 取回 scanout，与 `LBS1` 参考解码器按同一整数缩放居中规则算出的整帧逐像素
比对；内核 `boot_screen` 与 compositor 任一方的缩放、居中或进度填充规则漂移都会失败。随后注入一次平板移动与一次按键，要求
compositor 报告各自的首个事件。它不测量时间，参考帧与比对函数由 `scripts/tests/test_verify_display.py` 约束；只在 AArch64+HVF 承担。

Wayland 门禁（`scripts/verify_wayland.py`，`make verify-runtime-wayland`）在同一 QEMU 显示会话里注入固定的上游
`wayland-info` 与本仓库的 `wayland-probe`（私有镜像，产品 rootfs 不含）：`wayland-info` 必须列出 `wl_compositor`、`wl_shm`（含格式）、
`xdg_wm_base` 与 `wl_output`（模式、scale、名字）；探针走完 xdg configure/ack、五帧 shm buffer 与 frame callback；`screendump` 里
窗口矩形是最后一帧颜色、其外是桌面背景、位置是层叠放置的第一个窗口；收到的 keymap 由 guest 里链接上游 libxkbcommon 的 `xkb-check`
（禁用默认 include，逐键与修饰键索引裁决）编译；注入平板移动/点击与 Shift+A，客户端必须按序收到 `wl_pointer` 的 enter/motion/button/leave
与 `wl_keyboard` 的 modifiers/key（局部坐标按输出 scale 换算）；最后注入 Alt+F4，窗口必须消失、桌面恢复。协议状态机另有
进程内测试（`user/compositor` 的 `cargo test`，含 socket、乱序/拆包/随机输入）。只在 AArch64+HVF 承担。

idle tick suppression 不增加 host microbenchmark：收益来自 HVF/TCG 的 whole-machine exit 次数，
host unit loop 无法代表它。改动必须通过双 architecture compile/static gate，并以单 QEMU、完整 SMP
拓扑的多窗口 host CPU 采样作诊断；secondary idle→IPI→task 与 timeout runtime gate 负责活性语义。

资源使用记账（`getrusage`）不新增 benchmark：用户态 trap 返回多一次 counter 读和一次本 CPU 私有 Relaxed 存储，
缺页多一次 Relaxed 读和一次原子加，上下文切换多两次原子加，tick 多一次原子加，都没有新增锁或分配。
guest 里 `write(/dev/null)` 紧循环 0.4 秒完成 441k 与 446k 次（改动前后），在噪声内；host 微基准无法代表 trap 往返，
user/system 拆分的纯算术由 `scheduler-unit` 单测，端到端由 musl smoke 的 `resource-usage.c` 裁决。

io_uring 当前切片不新增 blocking benchmark：同步路径是对共享页的有界内存循环加一次无竞争的 `TaskMutex` 快路径，CQE 发布是一次 `spin::Mutex` 内的
16 字节写与一次 release 存储，稳态没有分配（溢出链表只在 CQ 满时增长，且有硬上限）。读写类 opcode 的成本由已有的 `read(2)`/`write(2)` 核心决定，新增部分只有
一次 NOWAIT 尝试。阻塞路径每次唤醒重新登记一次 `wait_for_poll`（O(挂起请求数) 个 key），挂起数量大时这是线性成本，但它只在请求真的阻塞时发生，
host 微基准测到的只是 key 构造，不代表 io_uring 的真实价值——批量提交摊薄 syscall 往返、阻塞请求不占线程。这两项要等 `POLL_ADD`/`TIMEOUT` 与块层异步落地、
有可比较的基线后再加入 blocking benchmark。
每条挂起的链是一个 `Box`ed future（fallible 分配）：不会等待的 op（普通文件、显式偏移、fsync、close、cancel、NOP）直接同步完成、不分配；
会等待的 op（pipe/socket 流式读写、poll、timeout）无论是否真的阻塞都分配一次 future，因为 future 必须在首次 poll 之前就位于堆上（poll 过的 future 不能搬动）。
持久文件读的新增成本：page cache 命中路径只多一次 `storage_epoch` 读取与一次 `NowaitRead` 状态构造（无分配）；
未命中每页多一个 `Box`ed `StorageRead`、一个 `PendingRead` 与预留的 cache 节点和页，这些原本也要分配。阻塞车道每个作业一次 `Box` 与一个结果槽 `Arc`。
host 微基准测不到块层真实重叠的收益（需要真实设备延迟），本次没有新增 benchmark，正确性由 host 单测与 guest 冷读检查点裁决。
链新增的热路径成本是每个 SQE 一次 `Member` 构造与每条链一次 `Ring::admit`（`spin::Mutex` 内的有序 `Vec` 追加，链完成时 `binary_search` 加 `remove`），
批内 SQE 不分配；这些在 `io_uring_enter` 的 syscall 开销面前可忽略，本次没有新增 benchmark，原因同上。正确性由 `io-uring.c` 烟测（提交/完成顺序、溢出、阻塞唤醒、取消、读写、epoll 通知）与 `abi.rs` 单测裁决，本次没有测得吞吐数字。

选核轮转不新增 benchmark：每次抢占只多一次本 CPU 私有计数的 Relaxed 读写和一次 Ready/Running 计数读取，选核仍是
O(CPU 数) 的单趟扫描，没有新增锁、分配或跨 CPU 访问；收益是公平性而非延迟，由 `verify-runtime-scheduler` 与
`scheduler-unit` 的策略单测裁决，host 微基准无法代表多 CPU 时间片轮转。

TLB shootdown 不增加 host wall-clock benchmark：成本主要来自 target firmware/IPI，宿主计时不能稳定代表它。
blocking gate 使用 production `TranslationCommit` 派生的确定性计数：
lazy mmap 为 `0 local / 0 remote`；1 MiB、256 页 first-touch 为 `256 local / 0 remote target`；
revoke/replace batch 为 `online_cpus - 1` 个 remote target；合并跨度不超过 64 页时执行精确逐页 fence，
超过 64 页时规范化为 1 次 full fence，防止稀疏 VMA teardown 把跨度页数变成无界指令循环。
RISC-V release target 继续验证 range `SFENCE.VMA` mechanism。AArch64/HVF 是明确的平台例外：
动态 TTBR1 kernel-stack retirement 的本地 31 页 range 固定强化为 1 次 `VMALLE1`，
避免 HVF 保留已经撤销的 global stack translation；
任一 remote revoke 先执行 full `VMALLE1IS`，再向每颗目标 vCPU 发送 SGI；目标以 barrier
越过 hypervisor flush point 并发布 generation ack，owner 在全部 ack 完成前不得释放
frame/page table。kernel-stack retirement 还必须在释放 `KERNEL_SPACE` lock 后等待，
避免 IRQ-masked target 等待同一锁形成环。该路径规避 broadcast 指令返回后其他 vCPU
仍持有 stale TTBR0/TTBR1 translation，不改变 publication/relax 的 local-only 热路径。
成本由 architecture static gate、四 CPU COW migration 与双 CPU kernel-stack churn
runtime gate 裁决，不用失真的 host wall-clock 阈值掩盖。

设备模型与驱动注册的间接层按成本归属分别裁决，不增加 host wall-clock benchmark：

- `DRIVER_IO` completion dispatch 是唯一位于每次块/RNG/声卡 completion 上的新增路径：completion 源存放在
  只追加的 `Once` 槽位，dispatch 无锁遍历、不克隆 `Arc`。`deferred_context_contract` 解析该函数并禁止
  `lock`/`clone`/`cloned`，mutation 测试证明恢复加锁会失败。
- 字符设备读写经一次 `dyn DeviceFile` 调用与 `UserOutput`/`UserInput` 游标：每次 syscall 一次虚调用，
  用户事务数由 `io_copy_cost` 的确定性计数固定（1 MiB `/dev/zero` 一次事务，evdev/DRM 每批 validate 与
  copy 各一次），不需要 wall-clock 阈值。
- 设备类注册表、字符/块设备注册表、mount 事务锁与 `JobControl`/`PipeScheduler` hook 只在 open、
  mount、启动装配或 TTY 访问判定上取得；TTY 判定在改造前已取 process graph lock，新增的只是一次 hook
  虚调用。这些都不在每字节或每 completion 路径上，只作诊断，不增加 blocking gate。
- 设备类 deferred vector 每个已发布 bit 一次函数指针调用，handler 表为只追加的 `Once` 槽位，无锁无分配。
- mmap 的 `MappedFile` 在每次映射建立时分配一次；page fault 是一次 `dyn SharedFileMapping` 调用，进入
  page cache 或 `MemoryFile`（按 `Inode::data_backing` 在建立映射时选定）。

inotify 的变更点不增加 benchmark：没有 watch 时每个 `notify_*` 是一次 Relaxed 原子读取；有 watch 时才做 inode
身份查询与一次注册表查找，成本归属于被监视的对象，不在无关文件的 read/write 路径上。

内存型文件（tmpfs、memfd）同样不增加 wall-clock benchmark：

- `MemoryFile` 的 read/write 在每次 syscall 取一次 `state` 锁，页查找是 `FallibleMap` 的 O(log n)；
  它替换的是先前 memfd 的单 mutex 字节缓冲，并去掉 tmpfs 经 page cache 的第二份拷贝，没有新增每字节路径。
  稀疏页、洞读零、短写与截断语义由 host 单元测试确定性裁决。
- tmpfs 目录的名字索引与 cookie 索引各一次 O(log n) 树操作；`getdents` 每项一次额外名字查找。
  树结构的比较次数上界已由 `FallibleMap` cost gate 固定，目录 cookie 的稳定性由 host 测试裁决。
- tmpfs 的 `namespace` 与 inode 状态锁只在创建、删除、rename、link 上取得；read/write 与 mmap 完全不经过它们。

VMA hot path 使用 deterministic structure gate，不增加受宿主调度影响的 wall-clock benchmark。
production `VmaIndexState` transition tests 覆盖 stack grow、split/protect/merge、fork/exec 与
unpublished rollback；AVL neighbor tests 对 16,384 entries 校验 comparison bound。
FallibleMap cost gate 还固定 RV64 iterator 不超过 16 bytes、4,096 项 persistent scan 至多一次
key comparison、随机 `BTreeMap::retain` 模型、successor-link 与 retained node identity；
architecture-check 禁止 production one-shot iterator lookup、fixed path stack、逐节点 retain
join 和 AF_UNIX detach 全图 retain。
architecture fence 要求 100 VMA、1 MiB prepare-user-write 为 0 次 stack 全扫描，hinted mmap 为 0 次 range/RLIMIT/push/merge 全扫描。

UDP/TCP ephemeral allocator 使用 16,384-bit 占用投影；固定 N=1,024 endpoint
样本下，旧 port-range×endpoint 模型为 16,777,216 次 endpoint probe，新模型为
0 次 endpoint probe，耗尽时最多读取 257 个 bitmap word。`port_namespace_cost`
同时阻止恢复 endpoint 扫描、丢失 exact-address 索引或 specific port-0 分类。

未初始化 input staging gate 以两个 64 KiB socket buffer、一个 1 MiB regular-write
buffer 和三个 1,024-fd pselect bitmap 为代表样本：copyin 前死写由 1,180,032 bytes 降为 0；
publication 必须经过同一 `UserInputStaging` initialized-prefix proof，禁止恢复预清零双轨。

## 架构与产物门禁

- `architecture-check` 校验 dependency matrix、concrete backend containment、global owner、unsafe proof、fallible collection、source size、ABI dispatch、userspace workspace 形态、`linux-uapi` 外 raw FFI 禁令、文档 fence 与 generated interface freshness。
- generated interface 只能由 checker 的 `--write-interface` 更新；任何差异都视为 architecture interface change。
- release kernel、需要时的 bootloader 和原生 userspace ELF 必须通过 target、segment、W^X、stack、interpreter、dynamic/RELRO 及 target-specific static-call 检查；kernel、rootfs、APK image 与所有构建 cache 必须属于同一个 architecture identity。

## 运行时门禁

完整验证从同一个只读 rootfs baseline 派生相互隔离的可写镜像，并覆盖：

- boot、CPU topology、interrupt、timer 与基础 filesystem；
- AArch64/HVF UTM VirGL 产品 GPU、keyboard、tablet、SPICE VirtIO Console 拓扑，以及空桌面启动链路：
  kernel GPU、输入、音频与 SPICE port 拓扑逐条发布 marker；当前不验证尚未实现的登录或桌面；
  首帧后继续要求固定启动的 Files 与 Terminal 各自完成连接、PTY shell 和应用首帧。gate 使用隐藏的
  disposable 一 CPU UTM 产品实例；无窗口 plain QEMU 不再承担图形通过判定，真实 11-CPU 全拓扑由
  headless SMP gate 与同一静态路径覆盖；
- 多 CPU 调度公平性：2 CPU 上 3 个、4 CPU 上 5 个忙循环，要求每个配置里最少者的 utime 至少达到最多者的 70%；
  任务被锁死在 100% / 50% / 50% 的分布比值约为 50%，公平分布接近 100%。计时取自 guest 内核的 `/proc/<pid>/stat`；
- AArch64/HVF 内核显示/GPU（不依赖 UI，见性能测试段）：VirGL2 能力、host GPU 清屏的像素读回，以及连续
  page-flip 的 vblank 间隔与丢帧；
- musl ELF/TLS/thread/signal/process consumer；process phase 在 sibling pthread 保持 runnable
  时执行普通 fork，并继续覆盖 wait、posix_spawn file actions 与并发 child waiter；
- 标准 Rust `std` 的 allocator/entropy、filesystem、Thread/TLS、process、AF_UNIX 与 IPv4 client；
- BusyBox init/ash、TTY、filesystem（含 tmpfs 的 mount 选项、link/rename、`size=`/`nr_inodes=` 配额与
  已删除但仍打开文件的空间归还）、IPC 与 network consumer；
- 内存型文件（memfd、tmpfs）的真实 guest 验证：`scripts/fixtures/musl/memory-file.c` 只注入一次性 gate 镜像，
  覆盖共享映射与 read/write 一致性、fork 共享、截断后映射 `SIGBUS` 与内容不复活、seal、`MAP_PRIVATE` COW、
  unlink 后映射仍可访问且空间在最后引用消失时归还，以及 `size=` 配额下 `ENOSPC`（write）与 `SIGBUS`（映射）；
- init 把 `/run`、`/tmp`、`/dev/shm` 挂成 tmpfs，所以 gate 经 `debugfs` 写入镜像的安装载荷与脚本只能放在持久的
  `/var/tmp`，不能放在 `/run`（会被 tmpfs 盖住）；
- APK 应用的 TLS/HTTP、SQLite journal/lock 和 Git object/ref/worktree vertical slice；
  curl timeout 后连续 64 次 fork 分别改写 parent/child shell state，再成功跨过下一次
  fork/exec，专门约束四 CPU migration 下的 COW remote TLB completion。
- 音频当前只有 kernel-unit 的 ALSA/混音测试与 boot/compile/static 覆盖；多路真实设备输出、amixer、
  Wine consumer 和延迟分析 gate 尚待实现，不能用 host 测试替代。

`verify-runtime` 与 `verify` 共用 `verify-runtime-gates`；它是 runtime 编排的唯一 owner，并串行运行各顶层门禁。
非图形门禁只走 headless QEMU；graphics boot 与 gpu 只走 disposable UTM VirGL，禁止同一能力
再由 plain QEMU 复制一套设备拓扑或通过判定。
每项仍使用独立镜像、success stamp 和 host port domain。APK 内部同样保持单一 QEMU owner：curl/Git 的
TLS/HTTP 竖切共用一台 4-CPU guest，SQLite 独占持久化/断电恢复镜像。guest 内被测的 SQLite
双 writer 与 curl 四路传输仍保持并发；`LITEOS_COW_MIGRATION_READY` 只有在 timeout 后
64 轮 parent/child 私有状态校验及下一次外部进程启动都成功后发布。SQLite writer A 必须在持有 `BEGIN IMMEDIATE` 后发布
guest 内握手，writer B 才能进入 blocking record-lock 路径。SQLite crash gate 保持一个
已 INSERT、未 COMMIT 的 WAL transaction，再在 guest 内精确 `SIGKILL` sqlite process；
重新打开后必须 `integrity=ok` 且未提交 row 不可见。它不把只覆盖 metadata 的 ext4 journal 物理掉电恢复
伪装成 SQLite 能力；QEMU SIGKILL 掉电仍由 filesystem 专项 gate 独立裁决。
SQLite 第一阶段 `sync` 后由 host 结束 VM，再冷启动同一持久化镜像执行恢复策略；禁止在 HVF
进程内用 system reset 串联两阶段，因为 QEMU 的 HVF exception handler 会触发 host `isv`
assertion。该生命周期边界不替代 guest `sync`、journal integrity 或 SQLite process-crash 门禁。
任一子门禁失败即整体失败，不能抽样或提前发布成功。

当前没有图形用户态应用或 UI 交互门禁；新 Wayland 会话按经典桌面计划实现后补齐验收。

`make verify-runtime-audio` 在 AArch64/HVF 上注入固定上游 amixer/alsa-lib 和 Linux UAPI fixture，
只写私有 rootfs。它覆盖 control 订阅/事件合并、errno、opener TID、dup/actual close、精确子设备选择，
随后用 amixer 调节 Master，并用八路不同频率的 alsa-lib consumer 显式启动播放。六阶段 WAV 分析分别
检查单路、八路、逐流增益、Master 左右增益、单声道静音和过载硬削波；缺音轨、错误增益和静音泄漏的
负例验证 analyzer 不会接受错误输出。时间戳通过真实 alsa-lib htimestamp 读取。
control 另以 delayed writer 验证阻塞 read 与 poll，PCM 以超过 buffer 容量的 WRITEI 实际阻塞；
两者都必须消费旧 notification token 后等待新的 level。该 gate 每次 cold boot，不使用成功缓存，串口与 WAV 保存在 `target/audio-runtime/`。
录音通过 QMP 的 monitor wavcapture/stopcapture 明确封口；QEMU 当前会输出弃用提示，未来移除该命令
需要迁移录音方式。直接终止仍持有 voice 的 WAV backend 不保证补写 RIFF 长度，不能修补文件头掩盖失败。
RISC-V secondary 只承担 compile、static/artifact、CPU topology boot 与 Rust std smoke。
自动门禁不生成 screenshot、preview 或视觉 Golden。

HTTPS origin 的 raw accept 与 TLS handshake 分属 server/connection worker owner；启动时持有
一个不发送 ClientHello 的连接，并要求 10 个合法握手在 2 秒总 deadline 内全部完成。该自检
阻止把 TLS 重新包装到 listening socket、恢复单连接 head-of-line blocking。

## 完整成功条件

默认 `make verify` 以 AArch64/HVF 作为 first-class 提交门禁，必须依次通过 format、clippy、
全部单元测试、blocking benchmark、release build、architecture/documentation fence、
artifact/static-call gate、全部 runtime gate 与 `git diff --check`。最后还必须以
`riscv64 + tcg + release` 执行 RISC-V secondary 的 compile、static/artifact 和 boot 门禁；
该 secondary 保留 backend 可构建与可启动契约，不冒充 AArch64 完整 runtime consumer 覆盖。

内核 consumer 的逐项覆盖见 [运行时回归覆盖](kernel-runtime-coverage.md)。
