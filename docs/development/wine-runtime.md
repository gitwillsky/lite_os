# Wine 运行时：构建、打包与安装

LiteOS 用上游 Wine 11.16 运行 Win32 程序，x86 部分由 Hangover 的 `libwow64fex.dll`（FEX 的 WoW64 DLL）模拟。
**日常不需要重新构建**：`prebuilt/wine/` 里是已构建好的运行时压缩包，安装到 rootfs 的 `/opt/wine`。只有升级 Wine、
改补丁或配置时才需要按下文重建并替换压缩包。

## 产物与来源

| 文件 | 内容 | SHA-256 |
|---|---|---|
| `prebuilt/wine/wine-11.16-aarch64-musl.tar.xz` | 裁剪后的 `/opt/wine` 运行时树（约 750 MB 展开、69 MB 压缩，Git LFS） | 见 `prebuilt/wine/SHA256SUMS` |
| `prebuilt/wine/cnc-ddraw-7.1.0.0.zip` | cnc-ddraw v7.1.0.0（红警 2 的 DirectDraw 兼容层，GDI 渲染） | 同上 |

- Wine 源码：`wine-11.16.tar.xz`，SHA-256 `c66e2090343dcd727f7f7fd2f87ee0bfb0b118790c1d745ab7b8a4c3a4197f2f`，来自 dl.winehq.org。
- 补丁（`third_party/wine/patches/`）：`01`–`08` 是 Hangover 11.16 的 8 个提交（AndreRH/wine@3ea3a149，不含版本字符串提交），
  缺少时 FEX 的 WoW64 DLL 因缺 32 位地址空间隔离与线程挂起接口，i386 进程加载 `kernel32.dll` 失败；
  `rpath.patch` 来自 Alpine，musl 的动态链接器不按 soname 复用已加载的 `ntdll.so`，unix 侧 `.so` 缺 `$ORIGIN` rpath 时进程初始化中止。
- `libwow64fex.dll`（ARM64 PE，与 libc 无关）随 Hangover 发布，已包含在压缩包的 `lib/wine/aarch64-windows/`，SHA-256
  `e1d58bbda509f080518b9910a33209e26a49c7050b97612793c8c2aa18f74ea8`；通过环境变量 `HODLL=libwow64fex.dll` 选用。
- musl 可用，不需要 glibc。

## 运行时依赖（Alpine 包）

Wine 的 unix 侧 `.so` 直接链接：`libasound.so.2`、`libffi.so.8`、`liblzma.so.5`、`libwayland-client.so.0`、`libxkbcommon.so.0`、
`libxkbregistry.so.0`、`libxml2.so.2`、`libz.so.1`；运行时 `dlopen` 的还有 FreeType、fontconfig、GnuTLS。构建它们的参照版本（Alpine 3.24.2 aarch64）：
`alsa-lib-1.2.15.3-r0`、`fontconfig-2.17.1-r1`、`freetype-2.14.3-r0`、`gnutls-3.8.13-r0`、`libffi-3.5.2-r1`、`libxkbcommon-1.13.1-r0`、
`libxml2-2.13.9-r2`、`wayland-libs-client-1.25.0-r0`、`xz-libs-5.8.4-r0`、`zlib-1.3.2-r1`（musl `1.2.6-r3`）。
**这些库尚未装进 LiteOS 的 rootfs**：需要经 `scripts/apk_cache.py` 的固定 SHA-256 机制加入（见"待办"）。

## 重新构建（仅在改版本、补丁或配置时）

参照环境是工程内 `.local/spikes/classic-desktop/` 的 Alpine 3.24.2 aarch64 虚拟机（QEMU + HVF，`scripts/vm-start.sh` 启动、`scripts/vm-ssh.sh` 登录；
没有这台机器时，任何 Alpine 3.24 aarch64 环境都可以，需要约 40 GB 空闲空间与 8 GB 以上内存，构建约 1–2 小时）。

1. 把 `third_party/wine/patches/` 拷到 guest 的 `~/wine-patches/`，把 `scripts/wine/*.sh` 拷到 guest。
2. 在 guest 里运行 `sh build-wine.sh`：校验源码哈希、打补丁、用 clang/lld 配置（`--enable-archs=aarch64,i386 --with-mingw=clang`，
   只启用 Wayland 显示与 ALSA 声音，关闭 X11、PulseAudio、OpenGL、Vulkan 等），安装到 `/opt/wine`。末尾打印 `BUILD_DONE`。
3. 运行 `sh package-wine.sh`：裁剪出运行时树（去掉头文件、静态库、man、开发工具与 PE 的调试信息）并打成 `.tar.xz`，打印哈希。
4. 把压缩包取回 `prebuilt/wine/`，更新 `SHA256SUMS` 与本文的表，`git lfs` 会处理它。
5. 构建 Wine 自带的 conformance 测试（`make -C build dlls/<名>/tests`）与基线记录见 `docs/plans/classic-desktop.md` 阶段 0。

构建时的注意事项：
- 缺 `gettext` 时 Wine 没有中文界面翻译，所以已加入依赖列表。
- 在 guest 里 `cp -a` 会因无法保留属主而失败，打包脚本用 `cp -R`。
- Wine 的 PE 文件按 64 KiB 文件对齐，未压缩体积很大，但压缩比很高。

## 安装到 rootfs

压缩包展开后顶层目录是 `wine-rt/`，其内容整体安装到 rootfs 的 `/opt/wine`（`bin/`、`lib/wine/{aarch64-unix,aarch64-windows,i386-windows}`、`share/wine/`）。
启动需要：`WINEPREFIX`（每用户，首次运行由 `wineboot` 初始化）、`WAYLAND_DISPLAY`、`XDG_RUNTIME_DIR`、`HODLL=libwow64fex.dll`、
`WINEDLLOVERRIDES="mscoree,mshtml="`（不带 Mono/Gecko）。

## 待办（阶段 6 余下部分）

- 把上面的 Alpine 运行时库加入固定 APK 闭包，并把压缩包安装进镜像的构建/同步流程；
- `scripts/wine_runtime.py` 与 runtime gate：在 LiteOS 上运行 notepad、winemine、winecfg，再对照 conformance 基线；
- 运行时验证 `0x10000`、`0x110000` 处约 1.7 GB 的 `PROT_NONE` 低地址预留与 `tkill` + `rt_sigreturn` 的线程挂起路径。
