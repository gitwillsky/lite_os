# mpv-client

- **类型**：库
- **作用**：上游 libmpv 的 Rust 客户端：属性、命令、事件与软件 render API
- **产物 / 安装位置**：播放器的内部依赖（静态链接）；上游 `/usr/lib/liteos/libmpv.so.2` 由 `scripts/mpv_runtime.py` 安装
- **对应的 Windows 2000 组件**：quartz.dll / DirectShow（媒体引擎）
- **依赖**：linux-uapi（`dynamic_library` 的 `dlopen`）、liteos-runtime

`Mpv::create` 经 `linux_uapi::dynamic_library` 载入 libmpv、设置选项、初始化并建立 `MPV_RENDER_API_TYPE_SW` 渲染上下文。
本 crate 不导入任何符号：C 函数表由 `dlsym` 解析，`extern "C"` 只出现在函数指针类型与回调里（架构围栏据此放行）。
mpv 的唤醒与帧更新回调只向一个非阻塞 `UnixStream` 写字节，调用方用 `AsyncFd` 等待它，再在自己的线程里 `poll`/`render`。
没有 libmpv（如 riscv64）时 `create` 返回 `NotFound` 并带加载器信息。
