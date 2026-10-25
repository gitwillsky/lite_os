# Rust UI SDK 开发

接口与所有权见[框架架构](../architecture/ui-framework.md)及[契约](../architecture-contract/ui-framework.md)。
SDK 和本机工具链已接入构建/安装入口；本机完整编译、二进制升级兼容和 GUI 行为尚未执行验收。

## 安装

开发镜像的准备入口见[构建与验证](build-and-verify.md#rust-sdk-开发镜像)，执行前停止旧 VM。

该入口同步系统运行库、SDK 和示例，随后用现有 APK 安装固定 AArch64 Rust/Cargo 1.87.0 与链接器。
软件包保留自身签名和摘要校验；工具链版本无需与系统 nightly 相同。
RISC-V 本机工具链不在当前安装入口的支持范围；共享 ABI 不因此改变。

## 在 LiteOS 中创建项目

```sh
cargo new --vcs none hello_ui
cd hello_ui
cargo add liteos-ui --path /usr/lib/liteos/sdk/liteos-ui
mkdir -p .cargo
cp /usr/share/liteos/sdk/template/.cargo/config.toml .cargo/config.toml
cp /usr/share/liteos/sdk/template/src/main.rs src/main.rs
cargo run
```

配置继续使用官方 Linux/musl target：动态链接系统 musl 和共享 UI 库，应用自行链接自己的 Rust std。
SDK 的 path dependency 不依赖网络，可直接 cargo build --offline；其他依赖按 Cargo 标准的 lockfile、
registry 或 vendor 规则准备，没有专用项目生成命令或第二个安装器。
实际代码以普通 Rust API 创建 Window 和 Widget；字体、像素缓冲、帧 callback、协议编码由运行库提供。

外部交叉开发也消费相同 SDK 源码和目标运行库，将项目 Cargo 配置中的 native library search path
指向目标 sysroot；链接器与标准库必须属于所选 Linux/musl target，不能混用 macOS 产物。

## 异步与关闭

- `Application::spawn_local` 接收非 Send Future，返回可 await 的 `Task<T>`；保留 Task owner，Drop 即请求取消。
- `Application::toplevels` 读取标准 foreign-toplevel 的完整快照，ToplevelsChanged 通知后重新查询；旧对象关闭后请求返回错误。
- `Application::events` 订阅控件动作；`stream.next().await` 等待下一次事件，控件或窗口关闭后结束。
- `Application::blocking` 将 Send 的自有数据交给有界后台池；等待结果期间不占用 UI 线程。
- `Application::clock` 与 `Application::background` 返回可复制入口，允许任务在 await 后继续创建等待或阻塞工作。
- `Application::dialogs` 返回可跨 await 使用的对话框入口；`dialogs.message(...).await` 与 `dialogs.file(...).await` 共用主循环。
- `FileFilter` 指定不含点前缀的扩展名，`dialogs.file_filtered(...).await` 按类型显示文件；目录始终显示，可切换所有文件。
- `WidgetKind::Tree` 接收父先子后的 `TreeRow`，刷新保留稳定 ID 的展开状态；应用通过 selection 查询当前选择。
- `WidgetKind::TextEditor` 提供多行编辑；`Widget::edit(EditCommand)` 发送剪切、复制、异步粘贴、删除、全选和插入动作。
- `Application::sleep` 使用统一 deadline；`AsyncFd` 使用标准非阻塞 I/O readiness，不表示 I/O 一定成功。
- `Application::clipboard_text` 等待完整 UTF-8 传输；需要超时参数，取消时关闭读端并撤销注册。
- 跨 await 优先持有 WeakWindow，更新 UI 前 upgrade；不持有控件可变借用或锁等待另一项操作。

事件流有界，溢出是显式错误；没有后台无限排队或忽略错误的成功结果。
回调内可请求关闭窗口，下一次分发清理取消任务；不嵌套运行第二个 UI 循环。

## 运行库更新

安装产物中的链接名为 libliteos_ui.so，运行时依赖为 libliteos_ui.so.1。
兼容更新保留 ABI 主版本、旧符号/布局和语义；应用重启后使用新实现。
不在正在运行的对象中热替换函数或原地覆盖已映射文件。资源与代码的兼容规则同样适用。

## 系统内终端

桌面“终端”入口运行 `/bin/terminal`，默认以当前账户的 HOME、USER、PATH 启动 `/bin/sh`。
也可以 `/bin/terminal /绝对路径/程序 参数...` 启动指定命令。PTY 使用标准 session/process-group、termios 与 Linux 终端编码。
Ctrl+Shift+C 复制选区，Ctrl+Shift+V 异步粘贴；滚轮浏览已有历史。窗口尺寸变化先准备模型存储，再提交 PTY 尺寸并发布模型。
终端使用 `TERM=linux`，运行 ncurses 应用时应通过现有 APK 安装对应的 `ncurses-terminfo-base`。
该入口与共享控件迁移已接入代码和安装清单，尚未进行 GUI 或 guest 运行验收。
