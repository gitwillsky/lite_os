# linux-uapi

- **类型**：库
- **作用**：Linux 内核接口封装
- **产物 / 安装位置**：共享库 `liblinux_uapi.so`
- **对应的 Windows 2000 组件**：kernel32.dll / ntdll.dll
- **依赖**：liteos-runtime

DRM、evdev、ALSA、PTY、memfd、进程、poll、SCM_RIGHTS 的 typed 封装；设备/libc 原始 FFI 的唯一 owner，公开 UI ABI 另由其专属 bindings 拥有。
filesystem 提供 statvfs 容量和 renameat2(RENAME_NOREPLACE) 原子不覆盖移动；非 Linux 返回 Unsupported。`src/bin/` 的 `session-launch`、`drm-flip-probe` 是辅助程序。
