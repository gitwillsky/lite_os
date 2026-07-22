# 固定规范与上游源码基线

本文件只维护不可变版本、commit、校验值和一手来源。当前能力与缺口不在这里声明；升级任何基线必须同时审计受影响的 ABI、实现与测试。

## 核心规范

### Linux arm64 + riscv64 ABI

- Linux `v7.1`；tag object `b3f94b2b3f3e51ab880a51fc6510e1dafba654ed`；peeled commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6`。
- 来源：[Linux commit](https://github.com/torvalds/linux/commit/8cd9520d35a6c38db6567e97dd93b1f11f185dc6)、
  [arm64 syscall UAPI](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/arch/arm64/include/uapi/asm/unistd.h)、
  [RISC-V syscall UAPI](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/arch/riscv/include/uapi/asm/unistd.h)、
  [asm-generic syscall UAPI](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/asm-generic/unistd.h)。
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

- `nightly-2026-07-12`；rustc commit `be8e82435eb04fbe75ed5286b52735366e160bed`；LLVM `22.1.8`。
- 来源：[固定 rustc commit](https://github.com/rust-lang/rust/commit/be8e82435eb04fbe75ed5286b52735366e160bed) 与仓库 `rust-toolchain.toml`。
- 标准 userspace 的 `std`、`panic_abort` 与静态 LLVM libunwind 都取自该 toolchain 安装的同 revision
  rust-src；不得从 host LLVM、滚动系统 libunwind 或另一 Rust revision 回退。

### smoltcp

- crate `0.13.1`；source commit `e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897`。
- 来源：[release 文档](https://docs.rs/crate/smoltcp/0.13.1)、[固定源码](https://github.com/smoltcp-rs/smoltcp/tree/e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897)。
- 只启用 alloc、Ethernet、IPv4、UDP、TCP 与 Reno；Linux socket UAPI 不由 smoltcp 定义。

### musl 与 BusyBox

- musl `v1.2.6`；commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`；tarball SHA-256 `d585fd3b613c66151fc3249e8ed44f77020cb5e6c1e635a616d3f9f82460512a`。
- 来源：[musl releases](https://musl.libc.org/releases.html)、[固定源码](https://git.musl-libc.org/cgit/musl/tree/?id=9fa28ece75d8a2191de7c5bb53bed224c5947417)。
- BusyBox `1.37.0`；tarball SHA-256 `3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4`。
- 来源：[BusyBox release](https://busybox.net/downloads/busybox-1.37.0.tar.bz2)。
- 两者是固定 consumer，不制定 kernel ABI，也不能把固定 smoke 外推为通用兼容性。

### ext2/JBD2

- on-disk 与 transaction 语义固定到 Linux `v7.1` 同一 commit。
- 来源：[JBD2 文档](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/Documentation/filesystems/ext4/journal.rst)、[固定实现](https://github.com/torvalds/linux/tree/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/jbd2)。

### Terminal

- JetBrains Mono `2.304`；source zip SHA-256 `6f6376c6ed2960ea8a963cd7387ec9d76e3f629125bc33d1fdcd7eb7012f7bbf`；SIL OFL 1.1。
- Medium/Bold TTF SHA-256：`44099e1efefba55637e0abbbf8dd3f526e59523345888a257bb01d39df4af74c`、`0198e841824025f8876e5c297f0b9b497ee8d6eb9969710a3328e1303f996ec3`。
- 来源：[官方 release](https://github.com/JetBrains/JetBrainsMono/releases/tag/v2.304)、[ECMA-48](https://ecma-international.org/publications-and-standards/standards/ecma-48/)。
- checked atlas SHA-256 为 `bbc87d129cbb440dd76eceef02755cb36d66e9a37f00ba01b615a4d7cb87abdd`；terminfo 使用 ncurses `6.5` source format。

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
- renderer 固定使用 cssparser `0.37.0`、Taffy `0.12.2`、Parley `0.11.0` 与 tiny-skia `0.12.0`；
  来源分别为 [cssparser](https://crates.io/crates/cssparser/0.37.0)、
  [Taffy](https://crates.io/crates/taffy/0.12.2)、[Parley](https://crates.io/crates/parley/0.11.0)、
  [tiny-skia](https://crates.io/crates/tiny-skia/0.12.0)。Taffy 只启用 Flexbox/block，Parley 禁止 system
  font discovery；精确 transitive source/checksum 由 `user/Cargo.lock` 唯一固定。

### Alpine、APK 与 TLS

- Alpine `v3.22/main/{aarch64,riscv64}` 只使用官方 repository 固定文件。AArch64 的
  `apk-tools-static 2.14.10-r0`、`alpine-keys 2.5-r0`、
  `ca-certificates-bundle 20260611-r0` SHA-256 依次为
  `3e22f80dd0272dc487e4ca84b2c6b660ca392cbad970764efe9ef9555b806ac8`、
  `2e4c85ae16cabeb53b4145006f883bf8e57d454bd3faff14d35ec7d8a0d05b1a`、
  `ae45c92eba28db3434058980c40930d3653663e5251cb04c9fd49a94ca00c93b`。
- RISC-V 的同三包 SHA-256 依次为
  `85419c4d80eceb12af9cc3be178dce3599ef04679c46eee25175b6673c14cd43`、
  `ca4835c8907791ab172fc64e53a81ab4ed06ff21c493d2a7fe8f66a80e2ea200`、
  `537dcb625ede1cb81e751dd92552b2715a35fdd72cdb43a965a055f14900d529`；curl/SQLite/Git
  应用闭包也由各架构的固定摘要完整锁定，禁止 latest 或跨架构推导。
- 本项目从官方 AArch64 repository 固定的闭包中，只有 `ca-certificates-bundle`、`git-init-template` 与 `ncurses-terminfo-base` 三个数据包以 `.PKGINFO arch=noarch` 发布；其余闭包必须是 `arch=aarch64`，该语义不是通用 `noarch` 豁免。
- OpenSSL `3.5.7`；commit `8cf17aaeb4599f8af87fefd810b5b5fee90fe69e`；tarball SHA-256 `a8c0d28a529ca480f9f36cf5792e2cd21984552a3c8e4aa11a24aa31aeac98e8`。
- 来源：[Alpine aarch64 repository](https://dl-cdn.alpinelinux.org/alpine/v3.22/main/aarch64/)、[Alpine riscv64 repository](https://dl-cdn.alpinelinux.org/alpine/v3.22/main/riscv64/)、[OpenSSL release](https://github.com/openssl/openssl/releases/tag/openssl-3.5.7)。

## 裁决顺序

1. privilege、CSR、page table 与 memory ordering：固定 RISC-V ISA。
2. S-mode 到 firmware：固定 SBI。
3. U-mode 到 kernel：固定 Linux arm64/riscv64 UAPI。
4. ELF、procedure call、TLS 与 relocation：RISC-V 使用固定 psABI；AArch64 只声明固定 Linux arm64 UAPI 与 artifact/runtime 门禁已验证的范围，不猜测未固定的 Arm 规范版本。
5. 标准函数和 utility：POSIX；musl/BusyBox 只作为 consumer proof。
6. 虚拟设备：固定 VirtIO normative requirements。

升级时必须固定新旧不可变 revision，并审计 number、UAPI layout/flags、ELF/TLS、trap/MMU、SBI extension、VirtIO state machine、consumer wrapper 与全部相关测试。
