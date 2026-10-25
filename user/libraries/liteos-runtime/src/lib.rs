//! 运行时库：把 Rust 标准库打包成共享库 `libliteos_runtime.so`。
//!
//! 每个系统库与程序都 `extern crate liteos_runtime as _;`，这样动态构建时 rustc 把 `std` 解析到这一个库，
//! 而不是各自静态内嵌一份；静态构建（探针、host 测试）里它只是一个空的 rlib。
pub use std;
