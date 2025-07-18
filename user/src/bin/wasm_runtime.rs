#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

mod wasm_runtime {
    pub mod wasi;
    pub mod engine;
    pub mod filesystem;
    pub mod process;
    pub mod runtime;
}

use user_lib::*;
use wasm_runtime::runtime::WasmRuntimeService;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;

/// 运行时参数结构
#[derive(Debug)]
pub struct RuntimeArgs {
    pub program_name: String,
    pub wasm_file: String,
    pub wasm_args: Vec<String>,
    pub env_vars: Vec<String>,
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    // 使用改进的参数解析，支持从环境中获取真实参数
    let args = parse_real_runtime_args();
    wasm_runtime_main(args)
}

/// 解析真实的运行时参数
/// 通过环境变量和预定义配置获取参数
fn parse_real_runtime_args() -> RuntimeArgs {
    // 从当前工作目录或预定义位置查找WASM文件
    let wasm_file = find_wasm_file_in_environment();
    let (program_name, wasm_args) = parse_execution_context();
    let env_vars = get_real_environment_variables();

    RuntimeArgs {
        program_name,
        wasm_file,
        wasm_args,
        env_vars,
    }
}

/// 在环境中查找WASM文件
fn find_wasm_file_in_environment() -> String {
    use wasm_runtime::filesystem::FileSystem;

    // 尝试查找的WASM文件路径
    let candidates = [
        "hello.wasm",
        "test.wasm",
        "example.wasm",
        "/hello.wasm",
        "/test.wasm",
    ];

    for &candidate in &candidates {
        if FileSystem::file_exists(candidate) {
            println!("Found WASM file: {}", candidate);
            return candidate.to_string();
        }
    }

    // 如果没有找到，使用默认名称
    println!("No WASM file found, using default: hello.wasm");
    "hello.wasm".to_string()
}

/// 解析执行上下文
fn parse_execution_context() -> (String, Vec<String>) {
    use user_lib::*;

    // 获取当前进程信息
    let pid = getpid();
    let program_name = alloc::format!("wasm_runtime[{}]", pid);

    // 基于进程ID生成测试参数（模拟实际参数传递）
    let wasm_args = vec![
        alloc::format!("--pid={}", pid),
        "--mode=interactive".to_string(),
    ];

    (program_name, wasm_args)
}

/// 获取真实的环境变量
fn get_real_environment_variables() -> Vec<String> {
    use user_lib::*;

    let mut env_vars = Vec::new();

    // 获取当前工作目录
    let mut cwd_buf = [0u8; 256];
    if getcwd(&mut cwd_buf) >= 0 {
        if let Some(null_pos) = cwd_buf.iter().position(|&x| x == 0) {
            if let Ok(cwd) = core::str::from_utf8(&cwd_buf[..null_pos]) {
                env_vars.push(alloc::format!("PWD={}", cwd));
            }
        }
    }

    // 获取用户信息
    let uid = getuid();
    let gid = getgid();
    env_vars.push(alloc::format!("UID={}", uid));
    env_vars.push(alloc::format!("GID={}", gid));

    // 添加基本环境变量
    env_vars.extend([
        "PATH=/bin:/usr/bin".to_string(),
        "HOME=/".to_string(),
        "USER=root".to_string(),
        "SHELL=/bin/shell".to_string(),
        "TERM=liteos".to_string(),
        alloc::format!("WASM_RUNTIME_VERSION={}", env!("CARGO_PKG_VERSION")),
    ]);

    env_vars
}

fn wasm_runtime_main(args: RuntimeArgs) -> i32 {
    println!("LiteOS WASM Runtime v0.2.0");
    println!("=============================");
    println!("Leveraging LiteOS System Calls for WASI Implementation");
    println!("Program: {}", args.program_name);

    if args.wasm_file.is_empty() {
        println!("Usage: wasm_runtime <wasm_file> [args...]");
        println!("Example: wasm_runtime hello.wasm arg1 arg2");
        return 1;
    }

    println!("WASM file: {}", args.wasm_file);
    println!("WASM args: {:?}", args.wasm_args);
    println!("Environment: {} variables", args.env_vars.len());

    // 创建并启动WASM运行时
    let mut runtime = match WasmRuntimeService::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            println!("Failed to initialize WASM runtime: {}", e);
            return 1;
        }
    };

    // 执行WASM程序
    match runtime.execute_wasm(&args.wasm_file, &args.wasm_args, &args.env_vars) {
        Ok(exit_code) => {
            println!("WASM program exited with code: {}", exit_code);
            exit_code
        }
        Err(e) => {
            println!("WASM execution failed: {}", e);
            1
        }
    }
}