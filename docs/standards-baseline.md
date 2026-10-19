# 固定规范与上游源码基线

本文件只维护不可变版本、commit、校验值和一手来源。当前能力与缺口不在这里声明；升级任何基线必须同时审计受影响的 ABI、实现与测试。

## 核心规范

### Linux arm64 + riscv64 ABI

- Linux `v7.1`；tag object `b3f94b2b3f3e51ab880a51fc6510e1dafba654ed`；peeled commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6`。
- 来源：[Linux commit](https://github.com/torvalds/linux/commit/8cd9520d35a6c38db6567e97dd93b1f11f185dc6)、
  [arm64 syscall UAPI](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/arch/arm64/include/uapi/asm/unistd.h)、
  [RISC-V syscall UAPI](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/arch/riscv/include/uapi/asm/unistd.h)、
  [asm-generic syscall UAPI](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/asm-generic/unistd.h)。
- io_uring 的 ABI（`include/uapi/linux/io_uring.h`）与语义（`io_uring/*.c`）取自同一固定源码树；本地以 `linux-7.1.tar.xz` 的 SHA-256 校验后解包，不另设来源。
- 该固定源码树定义共享 syscall number、architecture UAPI layout/flags、return 与 Linux errno。POSIX 和 libc wrapper 不能替代该层；本基线不声明未经固定一手 revision 的 Arm ABI 或 architecture 版本号。

### RISC-V ELF psABI

- `1.1` pre-release；tag `draft-20260701-e03d44ae2f0e1144f9498c2896b5ae25b0449398`；commit `e03d44ae2f0e1144f9498c2896b5ae25b0449398`。
- 来源：[固定源码](https://github.com/riscv-non-isa/riscv-elf-psabi-doc/tree/e03d44ae2f0e1144f9498c2896b5ae25b0449398)。
- 它定义 RV64 ABI、ELF、relocation、dynamic linking、TLS 与 calling convention；不定义 Linux syscall convention。

### RISC-V Privileged Architecture

- 官方发布包 `v20260120`；Machine ISA 与 Supervisor ISA 均为 `1.13`。
- 来源：[固定 HTML](https://docs.riscv.org/reference/isa/v20260120/priv/priv-index.html)、[固定 PDF](https://docs.riscv.org/reference/isa/v20260120/_attachments/riscv-privileged.pdf)。
- 它定义 privilege、CSR、trap/return、interrupt、PMP、Sv39/PTE 与 fence。锁、页表发布和 DMA 还必须对照同发布包的 A extension 与 RVWMO。

### SBI

- SBI `v3.0`；commit `c33ad9f414505806f084e8677e04d2744f76c8df`。当前 firmware 对外报告的 ABI version 是 `2.0`。
- 来源：[固定源码](https://github.com/riscv-non-isa/riscv-sbi-doc/tree/c33ad9f414505806f084e8677e04d2744f76c8df)、[官方入口](https://docs.riscv.org/reference/sbi/intro.html)。
- 它定义 S-mode 与 SEE 的 EID/FID、argument、`sbiret`、hart mask 与 extension，不定义 U-mode syscall。

### POSIX

- POSIX.1-2024 / Issue 8；publication id `9799919799`。
- 来源：[固定 edition](https://pubs.opengroup.org/onlinepubs/9799919799.2024edition/)。
- 它定义用户可观察的标准函数与 utility 语义；不定义 Linux number、register ABI 或 Linux-private object。

### VirtIO

- Virtual I/O Device `1.4` Committee Specification 01；source commit `917e900e0246b7fe21cdde795b0e566dd4f57d8d`。
- 来源：[CS01 HTML](https://docs.oasis-open.org/virtio/virtio/v1.4/cs01/virtio-v1.4-cs01.html)、[固定源码](https://github.com/oasis-tcs/virtio-spec/tree/917e900e0246b7fe21cdde795b0e566dd4f57d8d)。
- 它定义 transport、feature negotiation、status、virtqueue ownership、notification、reset 与 device conformance。

### Arm PrimeCell UART

- PL011 Technical Reference Manual `DDI 0183G`，PrimeCell UART revision `r1p4`。
- 来源：[Arm 固定手册](https://documentation-service.arm.com/static/5e8e3655fd977155116a9042)。
- 它定义 UART register layout、reset state、FIFO 与 interrupt threshold；AArch64 QEMU `virt`
  backend 不从 QEMU 行为反推这些 machine facts。

## 实现与 consumer 基线

### Rust toolchain

- `nightly-2026-10-03`；rustc commit `0abfedbc7cd4e725f126913880c95800394f7c37`；LLVM `23.1.1`。
- 来源：[固定 rustc commit](https://github.com/rust-lang/rust/commit/0abfedbc7cd4e725f126913880c95800394f7c37) 与仓库 `rust-toolchain.toml`。
- 标准 userspace 的 `std`、`panic_abort` 与静态 LLVM libunwind 都取自该 toolchain 安装的同 revision
  rust-src；不得从 host LLVM、滚动系统 libunwind 或另一 Rust revision 回退。
- AArch64 musl compiler runtime 的唯一外部补充是 LLVM `llvmorg-23.1.1` compiler-rt
  `lib/builtins/multc3.c` 及其 include 闭包，逐文件 SHA-256 固定在 `scripts/verify_musl.py`；
  该 toolchain 的 `compiler_builtins` 只以 `extern "Rust"` 导出 f128 complex helper。
- 来源：[LLVM compiler-rt builtins（llvmorg-23.1.1）](https://github.com/llvm/llvm-project/tree/llvmorg-23.1.1/compiler-rt/lib/builtins)。

### smoltcp

- crate `0.13.1`；source commit `e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897`。
- 来源：[release 文档](https://docs.rs/crate/smoltcp/0.13.1)、[固定源码](https://github.com/smoltcp-rs/smoltcp/tree/e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897)。
- kernel/host/benchmark 的唯一编译来源是 `vendor/smoltcp`；原 release 每个文件的 SHA-256
  记录于 `UPSTREAM.sha256.json`，保留原许可证与 upstream commit。Cargo 不保留 registry 备选路径。
- 本地变化限定UDP/TCP/raw transport、UDP ingress及interface dispatch与IPv4地址mechanism十个文件（第十个只是 `wire/udp.rs` 里 6LoWPAN 专用函数的 feature 门控）：connected peer 在 ingress admission 前匹配，canonical local
  endpoint 修改保留已有 queue，bind/rehash order 选择单播目标，广播/组播交付实际匹配集合，
  TX packet 在 enqueue 时冻结source port/address/TTL/oif；TCP实际MSS cache约束广告及分段。
  bound device在ingress准入前筛选，kernel唯一route selector在dispatch前选设备；IPv4
  weak-host tuple不因当前iface source列表而reset。Linux bind/shutdown/rlimit/netns UAPI仍归kernel owner。
- 只启用alloc、Ethernet、IP-medium实际loopback、IPv4、raw、UDP、TCP与Reno；Linux socket UAPI不由smoltcp定义，IPv6仍延期。

原生IPv4地址/proc NET归属补充依据同一固定Linux commit的
[`devinet_ioctl/inet_abc_len`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/devinet.c)、
[`get_proc_task_net/seq_open_net`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/proc/proc_net.c)、
[`dev_seq_show`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/core/net-procfs.c)
及kernel/nsproxy.c、kernel/kthread.c的真实引用创建/退出；不引入第二个规范版本。

NETLINK_KOBJECT_UEVENT的域、端口和过载以同一固定Linux commit的
[`netlink_bind/autobind/broadcast_deliver/overrun`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/netlink/af_netlink.c)
与[`uevent_net_init`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/lib/kobject_uevent.c)为依据。
端口按NET/协议独立，0首先使用调用者可见TGID，碰撞在[-2147483648,-4097]选择空位；
group0及同port重bind合法，protocol group数以外的mask被截断。
无tag内核事件发往所有owning initial UserNS的NET；非initial UserNS的NET不订阅该全局广播。
actual allocation收费覆盖每个receiver FIFO节点；overrun一次发表ENOBUFS，已有记录不替换，
SO_ERROR消费不解除拥塞，实际queue排空后恢复。

接口MTU继续使用上述固定dev_ioctl.c的dev_set_mtu调用与devinet.c的
inetdev_event/NETDEV_CHANGEMTU语义：lo允许0..65536，实际Ethernet设备允许68..1500
（本驱动frame storage上限1514）。低于IPv4最低68时销毁地址成员；仅恢复MTU不还原旧
地址，lo再次DOWN→UP才发表127.0.0.1/8。smoltcp interface caps只是该实际设备配置的
分段mechanism投影，同一NET事务同步刷新，不作为第二份可配置状态。

### musl 与 BusyBox

- musl `v1.2.6`；commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`；tarball SHA-256 `d585fd3b613c66151fc3249e8ed44f77020cb5e6c1e635a616d3f9f82460512a`。
- 来源：[musl releases](https://musl.libc.org/releases.html)、[固定源码](https://git.musl-libc.org/cgit/musl/tree/?id=9fa28ece75d8a2191de7c5bb53bed224c5947417)。
- BusyBox `1.37.0`；tarball SHA-256 `3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4`。
- 来源：[BusyBox release](https://busybox.net/downloads/busybox-1.37.0.tar.bz2)；下载优先使用内容相同的[阿里云镜像](https://mirrors.aliyun.com/slackware/slackware64-current/source/installer/sources/busybox/busybox-1.37.0.tar.bz2)，失败时回退官方源。
- 两者是固定 consumer，不制定 kernel ABI，也不能把固定 smoke 外推为通用兼容性。

### ext4/JBD2

- on-disk 与 transaction 语义固定到 Linux `v7.1` 同一 commit；feature profile 固定为 e2fsprogs
  `1.47.4` `mke2fs -t ext4` 默认值加原生 `ea_inode`，crc32c、half_md4 dirhash 与 checksum 公式以该版本
  `lib/ext2fs` 为交叉校验对象。
- 来源：[ext4 disk layout](https://github.com/torvalds/linux/tree/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/Documentation/filesystems/ext4)、
  [固定 ext4 实现](https://github.com/torvalds/linux/tree/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/ext4)、
  [e2fsprogs v1.47.4](https://git.kernel.org/pub/scm/fs/ext2/e2fsprogs.git/tag/?h=v1.47.4)、
  [JBD2 文档](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/Documentation/filesystems/ext4/journal.rst)、
  [固定 JBD2 实现](https://github.com/torvalds/linux/tree/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/jbd2)。

### Terminal

- JetBrains Mono `2.304`；source zip SHA-256 `6f6376c6ed2960ea8a963cd7387ec9d76e3f629125bc33d1fdcd7eb7012f7bbf`；SIL OFL 1.1。
- Medium/Bold TTF SHA-256：`44099e1efefba55637e0abbbf8dd3f526e59523345888a257bb01d39df4af74c`、`0198e841824025f8876e5c297f0b9b497ee8d6eb9969710a3328e1303f996ec3`。
- 中文 fallback 使用 Noto Sans CJK SC Regular/Bold，SHA-256 分别为
  `2c76254f6fc379fddfce0a7e84fb5385bb135d3e399294f6eeb6680d0365b74b`、
  `b5f0d1a190a7f9b43c310a8850630af12553df32c4c050543f9059732d9b4c0a`。
- 来源：[JetBrains Mono 官方 release](https://github.com/JetBrains/JetBrainsMono/releases/tag/v2.304)、
  [Noto CJK 官方仓库](https://github.com/notofonts/noto-cjk)、
  [ECMA-48](https://ecma-international.org/publications-and-standards/standards/ecma-48/)。
- Unicode cell width 使用 unicode-width `0.2.2`，checksum
  `b4ac048d71ede7ee76d585517add45da530660ef4390e49b098733c6e897f254`。
- checked atlas SHA-256 为 `dab03e4beb2ee1b5ac689bc3a608461530ce58774557d011881f782f8a892d61`；
  terminfo 使用 ncurses `6.5` source format。

### LiteUI

- QuickJS `2026-06-04`；官方 tarball SHA-256
  `b376e839b322978313d929fd20663b11ba58b75df5a46c126dd19ea2fa70ad2a`。来源：
  [官方 release](https://bellard.org/quickjs/quickjs-2026-06-04.tar.xz)。只编译 vendored original C source，
  不使用 fork、系统 library、JIT 或构建时下载。
- React `19.2.7`、react-reconciler `0.33.0`、esbuild `0.28.1`；npm integrity 分别为
  `sha512-HNe9WslTbXmFK8o8cmwgAeJFSBvt1bPdHCVKtaaV+WlAN36mpT4hcRpwbf3fY56ar2oIXzsBpOAiIRHAdY0OlQ==`、
  `sha512-KetWRytFv1epdpJc3J4G75I4WrplZE5jOL7Yq0p34+OVOKF4Se7WrdIdVC45XsSSmUTlht2FM/fM1FZb1mfQeA==`、
  `sha512-HrJrvZv5ayxBzPfwphOoNzkzOIIlifzk0KJrGK2c8R4+LKpMtpYLQeUdjnwjWv/LZlkH2laZk+4w78pi99D4Vw==`。
  来源：[npm React](https://www.npmjs.com/package/react)、
  [npm react-reconciler](https://www.npmjs.com/package/react-reconciler)、
  [npm esbuild](https://www.npmjs.com/package/esbuild)。唯一解析 owner 是 `ui/package-lock.json`。
- renderer 固定使用 cssparser `0.37.0`、Taffy `0.12.2`、Parley `0.11.0`、tiny-skia `0.12.0`
  与 unicode-width `0.2.2`；
  来源分别为 [cssparser](https://crates.io/crates/cssparser/0.37.0)、
  [Taffy](https://crates.io/crates/taffy/0.12.2)、[Parley](https://crates.io/crates/parley/0.11.0)、
  [tiny-skia](https://crates.io/crates/tiny-skia/0.12.0) 与
  [unicode-width](https://crates.io/crates/unicode-width/0.2.2)。Taffy 只启用 Flexbox/block，Parley
  禁止 system font discovery；精确 transitive source/checksum 由 `user/Cargo.lock` 唯一固定。

### UTM/QEMU host display、clipboard 与 SPICE agent

- QEMU `11.0.2`；tag commit `e545d8bb9d63e9dd61542b88463183314cff9482`；官方 tarball SHA-256
  `3745f6ea88e2e87fe0dc838b2b1d4e0a770bf48e01a1d5a186842a1fff76ccf5`。来源：
  [官方 tarball](https://download.qemu.org/qemu-11.0.2.tar.xz)、
  [固定源码](https://gitlab.com/qemu-project/qemu/-/tree/e545d8bb9d63e9dd61542b88463183314cff9482)。
- spice-protocol `0.14.5`；官方 tarball SHA-256
  `baf58449f6e89d19f475899ad5fb9196fdc46c03cc53233f4e39cf2978f9cff7`。来源：
  [官方 tarball](https://www.spice-space.org/download/releases/spice-protocol-0.14.5.tar.xz)。
- macOS 图形产品固定 [UTM v4.7.5](https://github.com/utmapp/UTM/releases/tag/v4.7.5)，只消费其
  official signed app、HVF、Metal/virgl 与 native SPICE `com.redhat.spice.0` PCI port；headless
  QEMU `11.0.2` 只服务验证/调试，不提供第二个 GUI backend。
- guest 只实现 spice-protocol 定义的 VDI chunk、`VD_AGENT_MONITORS_CONFIG`、capability 与 UTF-8
  clipboard message；显示请求通过标准 DRM/KMS CVT modeset，不引入 UTM 私有 guest ABI。
  QEMU/SPICE 不定义 LiteUI focus policy、display-protocol request identity 或 kernel devfs ABI。

### 媒体解码

- Symphonia `0.6.0`；source commit `980bf5830a90e069fd64641d9c38f067ab772a24`；
  顶层 crate SHA-256 `1758d6c853020a7244de03cc3e0185eaea3f58715122422dd3cc7452e6d4c16a`；
  MPL-2.0。来源：[固定源码](https://github.com/pdeljanov/Symphonia/tree/980bf5830a90e069fd64641d9c38f067ab772a24)、
  [crate release](https://crates.io/crates/symphonia/0.6.0)。
- `user/Cargo.lock` 唯一固定全部拆分 codec/format/metadata crate 与 transitive checksum。LiteUI 禁用
  default feature，只启用 `all` 与 AArch64 可用的 NEON optimization；不启用 experimental
  subtitle/video feature，不加载 system codec、plugin、FFmpeg executable 或运行时下载内容。
- Symphonia 只定义 container/codec/metadata 实现，不定义 HTMLMediaElement、autoplay、File/blob、
  ALSA、mixer 或设备 ABI。

### Alpine、APK 与 TLS

- Alpine `v3.22/main/{aarch64,riscv64}` 通过中科大镜像获取固定 bootstrap 输入；guest runtime
  repository policy 精确启用同镜像的 `v3.22/main` 与 `v3.22/community`，不启用 edge/testing。
  固定 bootstrap repository
  固定文件，下载后必须通过固定 SHA-256 校验。AArch64 的
  `apk-tools-static 2.14.12-r0`、`alpine-keys 2.5-r0`、
  `ca-certificates-bundle 20260909-r0` SHA-256 依次为
  `69f8d3f981440d9f2b69771b2718cc038c50e73cdbbecbc4416430ffc29ed70a`、
  `2e4c85ae16cabeb53b4145006f883bf8e57d454bd3faff14d35ec7d8a0d05b1a`、
  `2309bfdee4fea4172d2c775e44608b6b204b729732f5a6da97a28c21357ed034`。
- RISC-V 的同三包 SHA-256 依次为
  `3b94df19b7d84aee21e6bab7c5689a9627067df44f83b96a8c172aada8013cc2`、
  `ca4835c8907791ab172fc64e53a81ab4ed06ff21c493d2a7fe8f66a80e2ea200`、
  `687a1d048b9f8212e9e429831ef0c77e4fa8f5fc3dd261ac4e392dc1a262c21f`；curl/SQLite/Git
  应用闭包也由各架构的固定摘要完整锁定，禁止 latest 或跨架构推导。
- 本项目从官方 AArch64 repository 固定的闭包中，只有 `ca-certificates-bundle`、`git-init-template` 与 `ncurses-terminfo-base` 三个数据包以 `.PKGINFO arch=noarch` 发布；其余闭包必须是 `arch=aarch64`，该语义不是通用 `noarch` 豁免。
- OpenSSL `3.5.7`；commit `8cf17aaeb4599f8af87fefd810b5b5fee90fe69e`；tarball SHA-256 `a8c0d28a529ca480f9f36cf5792e2cd21984552a3c8e4aa11a24aa31aeac98e8`。
- 来源：[Alpine aarch64 repository（中科大镜像）](https://mirrors.ustc.edu.cn/alpine/v3.22/main/aarch64/)、[Alpine riscv64 repository（中科大镜像）](https://mirrors.ustc.edu.cn/alpine/v3.22/main/riscv64/)、
  [OpenSSL release](https://github.com/openssl/openssl/releases/tag/openssl-3.5.7)；
  构建优先从阿里云/清华 Gentoo distfiles 镜像获取同一 tarball，官方 release 兜底，均以上述 SHA-256 裁决。
  固定 APK 先从中科大镜像下载，失败时回退官方 `dl-cdn.alpinelinux.org`，同样以固定 SHA-256 裁决。

## 裁决顺序

1. privilege、CSR、page table 与 memory ordering：固定 RISC-V ISA。
2. S-mode 到 firmware：固定 SBI。
3. U-mode 到 kernel：固定 Linux arm64/riscv64 UAPI。
4. ELF、procedure call、TLS 与 relocation：RISC-V 使用固定 psABI；AArch64 只声明固定 Linux arm64 UAPI 与 artifact/runtime 门禁已验证的范围，不猜测未固定的 Arm 规范版本。
5. 标准函数和 utility：POSIX；musl/BusyBox 只作为 consumer proof。
6. 虚拟设备：固定 VirtIO normative requirements。

升级时必须固定新旧不可变 revision，并审计 number、UAPI layout/flags、ELF/TLS、trap/MMU、SBI extension、VirtIO state machine、consumer wrapper 与全部相关测试。

IPv4 FIB接入的源码依据同一固定Linux v7.1 commit的
[`rtentry_to_fib_config/fib_add_ifaddr`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/fib_frontend.c)、
[`ip_route_output_key_hash_rcu`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/route.c)、
[`fib_table_insert/delete/route_seq_show`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/fib_trie.c)。
Kernel拥有唯一33前缀bucket FIB及地址/路由事务，库只借用当前dispatch的next-hop/logical iif/
broadcast和当前ingress的LOCAL/broadcast/spec_dst事实，结束即清除。src/iface/mod.rs只导出
这些机制类型；src/phy/mod.rs的PacketMeta.interface_index是actual lo queue逻辑入口，
不定义用户ABI。原inventory保持固定，允许变化的准确范围是src/socket/{udp,tcp,raw}.rs、
src/iface/interface/{mod,ipv4,udp,tcp}.rs、src/iface/mod.rs、src/phy/mod.rs。
IP replies复用原Packet codec，以有预算的唯一kernel队列经相同FIB/neighbor dispatch；ARP
仍归当前link。没有为kernel维护per-interface library route mirror。

TCP parser实际匹配socket后冻结bound device，SYN握手使用同一FIB的反向出口MTU；仅
unbound或与logical iif相同的bound socket能匹配，所以当前RX只需要这两个实际route结论，
不建立每socket另一张FIB。无反向route的listener SYN不创建半连接。即时reply queue按该
实际bound oif重查同一FIB。ARP target LOCAL与on-link neighbor也来自同一FIB；依据
[`arp_process/arp_ignore`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/arp.c)
原生默认weak-host及RFC2131 probe，0源仅回复，不缓存0，logical iif不改变物理ARP link。

IPv4 route metrics与TCP纯时间phase已补源码：actual FIB保存原生ADVMSS、WINDOW、
RTT，ioctl的MTU转换为ADVMSS而不修改设备MTU；proc从同records显示三个metrics。
IpRouteContext只在本次active-open/egress/RX guard有效，reverse route提供被动握手事实，
结束清除。接收广告默认下限256/上限65495，发送MSS受实际MTU/peer/握手user cap约束；
两者不能合并，否则小MTU会错误增大发送段或降低原生SYN广告。窗口clamp限制真实SYN/RX，
route RTT只在握手完成seed首个data RTO，不伪造srtt采样。pure maintain先于route/NIC筛选，
无route/设备DOWN不阻止timeout与60秒TIMEWAIT退休；真实FailureReason产生一次ETIMEDOUT，
退休保留queued RX/FIN。无新锁/global/unsafe/分配；沿用实际TCP每slot成本门禁。
规范依据同一固定Linux commit的
[`rtentry_to_fib_config`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/fib_frontend.c)、
[`ip_metrics_convert`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/metrics.c)、
[`tcp_connect_init/tcp_advertise_mss`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/tcp_output.c)、
[`tcp_init_metrics`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/tcp_metrics.c)
及include/net/tcp.h的TCP_RTO_MIN/MAX/TCP_TIMEWAIT_LEN。source/wire/nativeguest/negative围栏
未声明的高级 TCP 选项与通用 PMTU/fragmentation 不支持。

TCP_SYNCNT/TCP_USER_TIMEOUT/TCP_LINGER2及reset退休已补源码。唯一transport纯时间
phase拥有实际pending sequence起点、RTO事件次数、前4次active SYN线性RTO及native模型预算；
无route/设备DOWN不冻结重试。USER_TIMEOUT仅约束未完成sequence，idle不会被强制关闭，
keepalive配置该值时仍须至少成功发出一个probe。数据orphan重试使用native默认安全预算8。
TCP_LINGER2默认getter60s、负值归一化-1、正值最多120s，期限仅在最后application fput后
生效；实际FIN ACK冻结FIN_WAIT2起点，最短3.5*RTO；正常期限静默退休，负值发一次reset。
RST原表示只由实际TCP构造，standalone dispatch和kernel RouteReplies均复用原Packet codec。
队列保留payload/bound oif，绝不持有TCP/port/endpoint；OOM、预算满和无route均不恢复身份，
符合tcp_send_active_reset的best-effort单次发布。删去reset专用orphan/node与retain_transport。
准入仍有64报文/256KiB实际预算，正常FIN/TIME_WAIT继续由真实transport持lease。
固定依据：[tcp.c](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/tcp.c)
的__tcp_close/tcp_done/TCP_LINGER2，[tcp_timer.c](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/tcp_timer.c)
的tcp_write_timeout/tcp_orphan_retries/tcp_keepalive_timer，以及
[tcp_output.c](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/tcp_output.c)
的tcp_send_active_reset/tcp_acceptable_seq及include/net/tcp.h的tcp_fin_time。
实际wire回归已补reset一次校验和/窗口0/端口提前复用、OOM/no-route释放，以及live SHUT_WR
无FIN期限、orphan -1/1/默认60/120s及IF DOWN退休；nativeguest option范围与负围栏已写。
zero-window budgets 由唯一 transport clock 管理；通用 PMTU/fragmentation 与其他未声明扩展不支持。

NET 链路硬件 fault 由实际设备 owner 记录；单物理 NIC 与 lo 共用一套 outcome。NetworkPoll唯一完整outcome同时持backlog、
8项actual-link TX-ready及8项actual-link首个typed诊断；没有“一个device错误覆盖整个Result”
分支。退出NET guard后同轮所有成功link的AF_PACKET writer edges仍交付，RX/cleanup/通知
backlog全部保留。删去stack入口任意设备take_error、全域pending_device_error与各socket
readiness上的global EIO。设备RX/TX callback错误只属于actual PHY counter/diagnostic；
Socket SO_ERROR必须来自实际协议事件。AF_PACKET直接发送依然获得所选device同步错误。
Ethernet错误/oversize-drop/submit-failure counter沿实际NET mutex独占的Cell写入，与底层
成功RX/TX counter一起投影标准/proc/net/dev；oversize未交付帧不计成功RX，不双计drop。
无新锁/global/unsafe；NetworkPoll栈上固定array，事件扫描仍沿8-interface批次。
依据同一固定Linuxcommit的
[virtio_net.c](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/drivers/net/virtio_net.c)
receive_buf/start_xmit：丢帧与FIFO故障计device stats，TX失败best-effort drop，不给任意socket
制造sk_err。实际host源码补A RX fault同时B completion/RX65budget、一次诊断/独立counter/
不污染UDP或TCP/SO_ERROR，proc标准16栏实际errors/drop；新增negative fence拒绝丢backlog/
固定零counter/不消费诊断。完整actual NET poll1024-slot每handle成本沿既有200ns门禁，不
更改阈值；设备terminal recovery/限频诊断和
后续完整multiNIC原生地址/rtnetlink/veth/迁移仍须继续审查。

PROBE0/USER_TIMEOUT继续补了源根修，全部未编译/未测试。实际Timer::ZeroWindowProbe拥有
可选实际deadline、退避delay与一次pending intent；send无clock时不拿uptime ZERO冒充起点。
纯clock定首次期限并推进真实probe事件，无route仍按native负route错误退避；资源拥塞500ms
重试且不增加退避。发送复用原TcpRepr/Packet，实际snd_una-1空ACK，不发送或确认stream字节。
实际PROBE0 count和第一probe_since只属于transport；有效窗口0应答清count并刷新下一probe，
不重置USER_TIMEOUT起点。只有真实ACK推进SND.UNA才重置未完成sequence epoch。活socket
默认15次无人应答probe预算；orphan依native alive使用8/0。USER只在probe真正开始后计期，
原生持续零窗口回复在未配置USER时不因总尝试次数被误杀。实际host源码补空probe wire、
USER1000ms与非进展ACK、无route+IFDOWN探测预算、持续零窗口应答；负围栏已写。依据同一
固定Linux tcp_input.c tcp_ack_probe/tcp_ack以及tcp_timer.c tcp_probe_timer和tcp_output.c
的tcp_send_probe0/tcp_xmit_probe_skb。九文件vendor profile未扩大，无新lock/global/unsafe；
字段受actual transport/NET guard独占；沿用实际每slot clock/完整poll200ns benchmark门禁。
ACK 准入上界是发送 high-water `send_seq_no`，与可回退的重传游标 `remote_last_seq` 分离；未发送的数据、SYN 与 FIN 不能被确认。keepalive 遇 route 失败时消耗一次 probe count，并安排后续 deadline，不伪造成功的 wire 发送。
zero-window 使用有界原生 timer；通用 PMTU/fragment 与其他未声明 TCP 扩展不支持。
