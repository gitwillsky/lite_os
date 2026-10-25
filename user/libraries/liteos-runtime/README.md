# liteos-runtime

- **类型**：库
- **作用**：运行时共享库
- **产物 / 安装位置**：`/usr/lib/liteos/libliteos_runtime.so`
- **对应的 Windows 2000 组件**：msvcrt.dll
- **依赖**：无

动态构建时承载 Rust `std`；其余库与程序都链接它，全系统只有一份标准库。静态构建（探针、host 测试）里它是空 rlib。
