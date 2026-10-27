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
开发镜像通过 `scripts/wine_runtime.py` 安装固定 Alpine v3.22 闭包（包名与 SHA-256 由该脚本唯一维护），
不要求与构建 VM 的 APK revision 相同。`apk_cache.py` 同时校验包名、版本与架构。
安装器从 `assets/classic/fonts/fontmap.ini` 生成 `/etc/fonts/conf.d/99-liteos-ui.conf`，将原生与 Wine 共用的
`/usr/share/liteos/fonts` 纳入 Fontconfig 扫描，并追加相同的字体家族回退候选；不覆盖应用自选字体或用户的 Wine 注册表。
除共享库和 fontconfig 配置外，必须安装 `xkeyboard-config-2.43-r0` 的 `/usr/share/X11/xkb` 数据：
Wine 11.16 的 `wayland_keyboard_init` 在注册 `wl_keyboard` listener 前调用
`rxkb_context_parse_default_ruleset`。合成器发送自包含 keymap，仍不能替代这一步默认 ruleset 解析；
省略它会使窗口收不到正常键盘输入。来源与固定包摘要见[规范基线](../standards-baseline.md)。

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

在本 worktree 构建 AArch64 内核并同步开发镜像；构建入口与参数的唯一 owner 是
[构建、测试与验证](build-and-verify.md)，通用入口见 [README](../../README.md#常用命令)。

`sync-userland` 在 AArch64 默认传入 `--wine`；`LITEOS_WINE=0` 会撤掉同步器拥有的 Wine 文件。
运行时库与配置展开到本 worktree 的 `target/wine-runtime/aarch64/`；固定归档仍放 `.local/inputs/`。
同步器安装、回读并校验完整文件清单后才发布镜像内指纹；不得在 QEMU 持有镜像时同步。

1. 运行 `make run-gui`，进入桌面；也可使用[桌面验证](desktop-verification.md)的 QEMU 显示拓扑。
2. UART 恢复 shell 的初始 HOME 是 `/`，测试 root 时先设置下列环境。其他登录用户使用自己的 HOME、
   WINEPREFIX 与 `/run/user/<uid>`。Wayland socket 是全局的 `/run/liteos/wayland-0`，不可误指向用户 runtime 目录。

   ```sh
   export HOME=/root
   export PATH=/opt/wine/bin:$PATH
   export WINEPREFIX=$HOME/.wine
   export XDG_RUNTIME_DIR=/run/user/0
   export WAYLAND_DISPLAY=/run/liteos/wayland-0
   export HODLL=libwow64fex.dll
   export WINEDLLOVERRIDES='mscoree,mshtml='
   wine cmd /c echo hello
   printf 'seed\r\n' > /var/tmp/wine-input.txt
   wine notepad 'Z:\var\tmp\wine-input.txt'
   wine winecfg
   ```

3. 人工验收：notepad 中 Ctrl+A 后输入新文本、Ctrl+S，关闭后从 UART 读回文件，必须与输入一致；
   不能把发送按键或截图当作编辑成功。检查任务栏按钮的最小化/还原，切换 winecfg 标签、操作控件并正常关闭。
   从桌面“运行”启动时，登录会话已经设置 HOME 与 Wayland 环境，可执行
   `env HODLL=libwow64fex.dll WINEDLLOVERRIDES=mscoree,mshtml= /opt/wine/bin/wine notepad`。

## 共享会话与当前验收边界

Wine 的 Linux 只读 section 会选用 `MAP_PRIVATE`，并依赖 Linux 未 COW 页对文件更新的可见性，见
[`map_file_into_view`](https://github.com/wine-mirror/wine/blob/wine-11.16/dlls/ntdll/unix/virtual.c)。
内核必须在读缺页时保留 canonical 文件页，首次写入才 COW；首次读就复制快照会使会话对象 id 与
键盘共享状态停在旧值。通用内存语义与 cleanup 的唯一说明见[内存契约](../architecture-contract/memory.md)。

命令行与非 UI 原生探针用于验证私有文件可见性、COW、fork、mprotect、truncate、默认 XKB context、
ruleset 解析和字符转换；最终编辑保存与控件交互仍需上述人工验收。Wine/FEX 继续消费上表中的固定归档。
完整 Wine conformance 对照、ddraw/dsound、低地址预留和线程挂起专项仍属于阶段 6 后续范围。
