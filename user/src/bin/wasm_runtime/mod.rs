//! LiteOS WASM Runtime - 模块声明
//!
//! 这是一个完整的WebAssembly运行时实现，充分利用LiteOS提供的POSIX兼容系统调用。
//!
//! ## 模块结构
//!
//! - `args`: 命令行参数和环境变量解析
//! - `wasi`: WASI接口实现，映射到LiteOS系统调用
//! - `engine`: WASM字节码解析和执行引擎
//! - `filesystem`: 文件系统操作封装
//! - `process`: 进程管理功能
//! - `runtime`: 运行时服务主模块
//!
//! ## 特性
//!
//! - 完整的WASM 1.0支持
//! - WASI 1.0标准接口实现
//! - 充分利用LiteOS的82个系统调用
//! - 模块化设计，易于扩展
//! - 完整的错误处理和调试支持

pub mod wasi;
pub mod engine;
pub mod filesystem;
pub mod process;
pub mod runtime;

// 重新导出主要类型和函数
pub use wasi::{WasiContext, WasiErrno};
pub use engine::{WasmEngine, WasmModule, WasmValue};
pub use filesystem::FileSystem;
pub use process::ProcessManager;
pub use runtime::{WasmRuntimeService, RuntimeStats, RuntimeConfig};

/// WASM运行时版本信息
pub const VERSION: &str = "0.2.0";

/// 支持的WASM版本
pub const SUPPORTED_WASM_VERSION: u32 = 1;

/// 支持的WASI版本
pub const SUPPORTED_WASI_VERSION: &str = "1.0";

/// 运行时特性标志
#[derive(Debug, Clone)]
pub struct RuntimeFeatures {
    /// 支持WASM核心规范
    pub wasm_core: bool,

    /// 支持WASI接口
    pub wasi_support: bool,

    /// 支持文件系统操作
    pub filesystem_access: bool,

    /// 支持进程管理
    pub process_management: bool,

    /// 支持信号处理
    pub signal_handling: bool,

    /// 支持内存管理
    pub memory_management: bool,

    /// 支持网络操作(未来扩展)
    pub network_support: bool,
}

impl Default for RuntimeFeatures {
    fn default() -> Self {
        Self {
            wasm_core: true,
            wasi_support: true,
            filesystem_access: true,
            process_management: true,
            signal_handling: true,
            memory_management: true,
            network_support: false, // 等待LiteOS网络支持
        }
    }
}

/// 获取运行时特性
pub fn get_runtime_features() -> RuntimeFeatures {
    RuntimeFeatures::default()
}

/// 检查特定特性是否支持
pub fn is_feature_supported(feature: &str) -> bool {
    let features = get_runtime_features();

    match feature {
        "wasm_core" => features.wasm_core,
        "wasi" => features.wasi_support,
        "filesystem" => features.filesystem_access,
        "process" => features.process_management,
        "signal" => features.signal_handling,
        "memory" => features.memory_management,
        "network" => features.network_support,
        _ => false,
    }
}

/// 打印运行时信息
pub fn print_runtime_info() {
    println!("LiteOS WASM Runtime {}", VERSION);
    println!("====================");
    println!("WASM Version: {}", SUPPORTED_WASM_VERSION);
    println!("WASI Version: {}", SUPPORTED_WASI_VERSION);

    let features = get_runtime_features();
    println!("Supported Features:");
    println!("  - WASM Core: {}", features.wasm_core);
    println!("  - WASI Support: {}", features.wasi_support);
    println!("  - Filesystem Access: {}", features.filesystem_access);
    println!("  - Process Management: {}", features.process_management);
    println!("  - Signal Handling: {}", features.signal_handling);
    println!("  - Memory Management: {}", features.memory_management);
    println!("  - Network Support: {} (Future)", features.network_support);

    println!("\nLiteOS System Calls Available:");
    println!("  - File Operations: open, read, write, close, lseek, stat, etc.");
    println!("  - Process Control: fork, exec, execve, wait, exit, etc.");
    println!("  - Memory Management: brk, sbrk, mmap, munmap");
    println!("  - Signal Handling: kill, signal, sigaction, pause, alarm");
    println!("  - IPC: pipe, dup, dup2, flock");
    println!("  - Permissions: getuid, setuid, chmod, chown, etc.");
}

/// 运行时初始化
pub fn init_runtime() -> Result<(), &'static str> {
    // 执行运行时初始化检查

    // 检查基本系统调用可用性
    let pid = user_lib::getpid();
    if pid < 0 {
        return Err("Basic system calls not available");
    }

    // 检查文件系统访问
    let mut cwd_buf = [0u8; 256];
    if user_lib::getcwd(&mut cwd_buf) < 0 {
        return Err("Filesystem access not available");
    }

    println!("WASM Runtime initialized successfully");
    println!("Current PID: {}", pid);

    Ok(())
}

/// 运行时清理
pub fn cleanup_runtime() {
    println!("WASM Runtime shutting down gracefully");
    // 执行清理操作
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature_support() {
        assert!(is_feature_supported("wasm_core"));
        assert!(is_feature_supported("wasi"));
        assert!(is_feature_supported("filesystem"));
        assert!(!is_feature_supported("unknown_feature"));
    }

    #[test]
    fn test_version_info() {
        assert_eq!(VERSION, "0.2.0");
        assert_eq!(SUPPORTED_WASM_VERSION, 1);
        assert_eq!(SUPPORTED_WASI_VERSION, "1.0");
    }
}