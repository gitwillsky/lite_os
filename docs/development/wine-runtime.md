# Wine 运行时：构建、打包与安装

LiteOS 用上游 Wine 11.16 运行 Win32 程序，x86 部分由 Hangover 的 `libwow64fex.dll`（FEX 的 WoW64 DLL）模拟。
**日常不需要重新构建**：`prebuilt/wine/` 里是已构建好的运行时压缩包，安装到 rootfs 的 `/opt/wine`。只有升级 Wine、
改补丁或配置时才需要按下文重建并替换压缩包。
Wine/FEX 当前仅交付 AArch64 运行时，不据此声明 RISC-V64 可运行 Win32 程序。

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
系统基础清单唯一发布 `user/base/fonts.conf` 到 `/etc/fonts/fonts.conf`，供 libmpv 与 Wine 共用；
Wine 只追加上游配置片段，并从 `assets/classic/fonts/fontmap.ini` 生成 `/etc/fonts/conf.d/99-liteos-ui.conf`，将原生与 Wine 共用的
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
5. Wine 自带的 conformance 测试按 `make -C build dlls/<名>/tests` 构建；对照范围与裁决规则见下文。

构建时的注意事项：
- 缺 `gettext` 时 Wine 没有中文界面翻译，所以已加入依赖列表。
- 在 guest 里 `cp -a` 会因无法保留属主而失败，打包脚本用 `cp -R`。
- Wine 的 PE 文件按 64 KiB 文件对齐，未压缩体积很大，但压缩比很高。

## 安装到 rootfs

压缩包展开后顶层目录是 `wine-rt/`，其内容整体安装到 rootfs 的 `/opt/wine`（`bin/`、`lib/wine/{aarch64-unix,aarch64-windows,i386-windows}`、`share/wine/`）。
统一公开启动入口为 `/bin/wine`。它根据当前登录账户设置唯一的 `$HOME/.wine`，选择 FEX 并启用
Wine 的 XDG 桌面集成，再 exec `/opt/wine/bin/wine`；普通应用和安装器不再自行拼装 Wine 环境。
安装器默认使用 `C:\Program Files`（32 位安装器也可能使用 `Program Files (x86)`），实际均位于
`$HOME/.wine/drive_c/`；其他账户拥有独立环境。

在本 worktree 构建 AArch64 内核并同步开发镜像；构建入口与参数的唯一 owner 是
[构建、测试与验证](build-and-verify.md)，通用入口见 [README](../../README.md#常用命令)。

`sync-userland` 在 AArch64 默认传入 `--wine`；`LITEOS_WINE=0` 会撤掉同步器拥有的 Wine 文件。
运行时库与配置展开到本 worktree 的 `target/wine-runtime/aarch64/`；固定归档仍放 `.local/inputs/`。
同步器安装、回读并校验完整文件清单后才发布镜像内指纹；不得在 QEMU 持有镜像时同步。

1. 在“我的电脑 → 控制面板 → 添加/删除程序”统一安装、卸载 Windows 应用。
   该入口运行 Wine 的标准 `appwiz.cpl`，直接使用 Windows 注册表的安装记录，不建立 LiteOS 应用数据库。
   也可以在资源管理器双击 `.exe` / `.msi`；MSI 关联到 `wine msiexec /i`，参数不经过 shell。
2. 安装时保留创建桌面快捷方式的选项。Wine `winemenubuilder` 把 Windows `.lnk` 导出为
   `~/Desktop/*.desktop`、`$XDG_DATA_HOME/applications/` 中的条目与 `icons/hicolor/*/apps/*.png`。
   桌面自动刷新并使用条目名称、图标、工作目录和 Exec；双击即可启动。开始菜单合并用户和系统程序。
3. 普通用户在图形登录会话内启动。UART 初始 HOME 为 `/`，需先 `export HOME=/root` 并设置
   `XDG_RUNTIME_DIR=/run/user/0`；启动器会拒绝把环境误建到根目录。例：`wine notepad`。
4. 人工验收必须使用明确的 i386 PE：编辑、Ctrl+S、关闭后读取文件，核对真实保存内容；
   再验证任务栏最小化/还原、Alt+Tab 与冷启动后的快捷方式。安装样本位于 `scripts/fixtures/wine/`。
   `make verify-runtime-wine` 在私有 AArch64/HVF 镜像执行安装、桌面启动、编辑保存及冷启动，不修改开发盘。
   首次初始化 phase 按 [Wine 11.16 run_wineboot](https://github.com/wine-mirror/wine/blob/wine-11.16/dlls/ntdll/unix/env.c) 的 5 分钟上限等待；窗口与输入仍使用显示门禁的默认预算。

不承诺安装器被强制限制在 Program Files；标准安装器仍可让用户选择目录。
没有创建快捷方式的便携程序不会凭扫描 EXE 自动注册；用户可通过其安装器或标准 Windows 快捷方式创建入口。

## 共享会话与当前验收边界

Wine 的 Linux 只读 section 会选用 `MAP_PRIVATE`，并依赖 Linux 未 COW 页对文件更新的可见性，见
[`map_file_into_view`](https://github.com/wine-mirror/wine/blob/wine-11.16/dlls/ntdll/unix/virtual.c)。
内核必须在读缺页时保留 canonical 文件页，首次写入才 COW；首次读就复制快照会使会话对象 id 与
键盘共享状态停在旧值。通用内存语义与 cleanup 的唯一说明见[内存契约](../architecture-contract/memory.md)。

命令行与非 UI 原生探针用于验证私有文件可见性、COW、fork、mprotect、truncate、默认 XKB context、
ruleset 解析和字符转换；最终编辑保存与控件交互仍需上述人工验收。Wine/FEX 继续消费上表中的固定归档。
完整 Wine conformance 对照、ddraw/dsound、低地址预留和线程挂起专项尚未完成，不能由命令行探针成功推断。

## Conformance 对照与游戏验收

| 范围 | 当前边界 | 验收要求 |
|---|---|---|
| 原生与 i386 conformance | 参照 Alpine 的阶段结果不等于 LiteOS 结果 | 分开记录 AArch64 原生与 FEX i386 的 kernel32、ntdll、user32、gdi32、ddraw、dsound 子测试；比较退出码、失败、跳过与超时 |
| 低地址与线程挂起 | Wine/FEX 的专项运行时验证尚未完成 | 验证 0x10000、0x110000 低地址的大段 PROT_NONE 预留，以及 tkill 与 rt_sigreturn 的停止/恢复路径 |
| 红色警戒 2 | 已准备 cnc-ddraw 归档；游戏部署、启动入口及验收尚未完成 | 用户提供游戏文件后部署兼容层与标准 Desktop Entry，实际进入遭遇战并有声音，验证退出与再次启动 |
| D3D | 当前 Wine 配方关闭 OpenGL/Vulkan | 标准 dma-buf/EGL 路径不代表 Wine D3D 可用；改变 Wine 配方后另行验证 |

参照基线的本地位置为 `.local/spikes/classic-desktop/out/baseline/*.tsv`，不随 clone 交付；
缺失时须用相同固定运行时重建，历史说明可从 Git 追溯。旧计划记录的以下干扰须重新复核，不能直接豁免 LiteOS 的新增失败：

- 原生 ntdll:exception 的调试器断点块不稳定，子进程块单独裁决；不能将任意新增失败或超时归入这一例外。
- FEX i386 的部分 loader、sync 与进程退出测试在参照环境也会挂起；ntdll:wow64 另有 xtajit 假设差异。
- headless 环境缺少输入设备或 GL，会导致 user32:input 与部分 ddraw 用例失败、跳过或挂起；
  对照必须记录实际设备条件，不能以跳过替代输入、声音和游戏验收。
