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
- rootfs 基线容量按发布文件总量推导，覆盖原树、APK 压缩包和原子替换峰值（最小 512 MiB）；APK 装配显式使用产品 `QEMU_MEMORY`（默认 2G），不能隐式消费 QEMU 的 128 MiB 默认值。
- rootfs 基线把同一个 BusyBox `env` inode 发布到 `/bin/env` 与 `/usr/bin/env` 并验证权限/identity，
  保证 community 中 npm/pnpm 等 `#!/usr/bin/env node` 入口安装后可执行。
- `FS_IMAGE_SIZE_MIB` 只控制可写开发实例的最小容量，默认 8192 MiB；`run`、`run-gui` 与
  `run-gdb` 在 QEMU 启动前离线扩容已有实例并保留内容，较大的实例不会被缩容。缺少该扩容会让
  基线派生的小容量实例在安装 Node.js 等应用时以 `ENOSPC` 失败；runtime gate 仍消费固定、
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
- `run-gui` 进入 logon 与用户桌面，UART 保留恢复 shell；首次账户设置及产品会话门禁见[桌面验证](desktop-verification.md)。
- UTM 手动删除后若残留注册，启动脚本会检查删除后的列表并停止，避免复用旧串口配置；退出并重新打开 UTM 后再运行。
  开发磁盘 `fs-${ARCH}.img` 独立于生成的虚拟机配置包，恢复注册不需要重建或删除开发磁盘。
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

Linux UAPI source generation 保持只读；`headers_install` 的 Kbuild `O=` 输出与安装结果都在本
worktree 的 `target/`。旧 in-tree 构建留下的 generated headers 不得被新构建复用，源码 cache
recipe 换代后从同一固定归档解包干净 generation，避免 VPATH 跳过目标树中的 asm wrapper。

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

### 构建缓存空间

- 日常开发保留 Rust incremental；根 workspace 与 `user/` 的 `test` profile 只生成
  `line-tables-only` 调试信息，保留回溯文件/行号，不保留完整类型与局部变量调试信息。
  `dev` 不覆盖，因此 `PROFILE=debug` 内核仍保留完整 GDB 调试能力；release 优化与验证基线不变。
- `build_cache.py` 唯一拥有镜像缓存保留策略：每个架构的 BusyBox rootfs 缓存及 APK 安装镜像
  各保留两份。构建/命中成功时保留本次使用项和最近一份历史，其余完整镜像与关联 stamp 自动删除。
  不完整条目、临时目录和符号链接不参与淘汰；缺失历史缓存只触发按固定输入重建。
- `.rootfs.lock` 同时保护 rootfs 缓存的发布、复制与淘汰，consumer 使用独立副本；
  `.installed.lock` 保护 APK 安装缓存的完整 gate 生命周期，包含安装、后续读取和淘汰。
  手动清理使用相同锁，异常退出自动释放；未完整发布的新产物不会触发淘汰。
- `make prune-cache-preview` 只列出待删除路径与已分配空间；`make prune-cache` 在各架构内
  保留最近两份并实际清理。不删除 Cargo 编译缓存、sysroot、`target/rootfs/` 当前基线、
  `fs-*.img` 开发实例、UTM 配置或 `.local/` 输入与实验资料。
- `make clean` 是完整 Cargo 清理，会删除根 `target/` 中的其他派生产物；日常释放历史镜像空间
  使用定向清理入口。关闭 `CARGO_INCREMENTAL` 不会删除已有缓存，并会增加后续源码修改的重编译量。

缓存淘汰只在构建发布阶段遍历镜像条目，不进入 kernel/userspace 热路径，无需新增运行时 benchmark。

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

## Rust SDK 开发镜像

宿主先停止旧 VM，再准备开发镜像：

```sh
source ~/.zshrc
make prepare-rust-development
```

该入口通过现有 APK 安装固定原生 Rust/Cargo、链接器与 SDK，不执行 guest 编译或 smoke。
进入 LiteOS 后按 [SDK 使用](ui-sdk.md)创建 Cargo 项目，应用构建不需要重新生成系统镜像。

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

稳定热路径采用阻塞式微基准或确定性结构门禁；真实设备与桌面延迟只作诊断，不用宿主耗时代替 guest FPS。
覆盖、成本边界与新增热路径的 benchmark 决策见[性能验证范围](performance-coverage.md)。

## 架构与产物门禁

- `architecture-check` 校验 dependency matrix、concrete backend containment、global owner、unsafe proof、fallible collection、source size、ABI dispatch、userspace workspace 形态、`linux-uapi` 外 raw FFI 禁令、文档 fence 与 generated interface freshness。
- generated interface 只能由 checker 的 `--write-interface` 更新；任何差异都视为 architecture interface change。
- release kernel、需要时的 bootloader 和原生 userspace ELF 必须通过 target、segment、W^X、stack、interpreter、dynamic/RELRO 及 target-specific static-call 检查；kernel、rootfs、APK image 与所有构建 cache 必须属于同一个 architecture identity。

## 运行时门禁

完整验证从同一个只读 rootfs baseline 派生相互隔离的可写镜像，并覆盖：

- boot、CPU topology、interrupt、timer 与基础 filesystem；
- AArch64/HVF UTM VirGL 产品 GPU、keyboard、tablet、SPICE VirtIO Console 拓扑，以及空桌面启动链路：
  kernel GPU、输入、音频与 SPICE port 拓扑逐条发布 marker；当前不验证真实登录或桌面交互；
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
- 音频的真实设备、amixer 和并发流门禁见下文；Wine consumer 与延迟分析不能用 host 测试替代。

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

当前图形门禁覆盖 compositor、Wayland 探针与外壳协议；真实登录、桌面应用与完整 UI 会话验收仍按经典桌面计划补齐。

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
