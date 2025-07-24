#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

mod wasm_runtime {
    pub mod engine;
    pub mod filesystem;
    pub mod process;
    pub mod runtime;
    pub mod wasi;
}

use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use user_lib::*;
use wasm_runtime::runtime::WasmRuntimeService;

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
    let args = parse_real_runtime_args();
    let exit_code = if let Some(args) = args {
        let result = wasm_runtime_main(args);
        result
    } else {
        println!("Usage: wasm_runtime <wasm_file> [args...]");
        1
    };

    exit_code
}

/// 解析真实的运行时参数
/// 通过命令行参数获取WASM文件名和参数
fn parse_real_runtime_args() -> Option<RuntimeArgs> {
    // 解析命令行参数
    let (program_name, wasm_file, wasm_args) = parse_command_line_args();
    if wasm_file.is_empty() {
        return None;
    }
    let env_vars = get_real_environment_variables();

    Some(RuntimeArgs {
        program_name,
        wasm_file,
        wasm_args,
        env_vars,
    })
}

/// 解析命令行参数
/// 从execve传递的参数中获取WASM文件名和参数
fn parse_command_line_args() -> (String, String, Vec<String>) {
    use user_lib::*;

    // 获取当前进程信息
    let pid = getpid();
    let program_name = alloc::format!("wasm_runtime[{}]", pid);

    // 获取真实的命令行参数
    // 我们需要实现一个函数来获取传递给进程的argc/argv
    let (wasm_file, wasm_args) = get_process_arguments();

    if wasm_file.is_empty() {
        println!("Usage: wasm_runtime <wasm_file> [args...]");
        // 返回空字符串，让主函数处理错误
        return (program_name, String::new(), vec![]);
    }

    (program_name, wasm_file, wasm_args)
}

/// 获取进程的命令行参数
/// 返回 (wasm_file, args)
fn get_process_arguments() -> (String, Vec<String>) {
    use user_lib::*;

    // 使用新的系统调用获取命令行参数
    let mut argc = 0usize;
    let mut argv_buf = [0u8; 1024];

    let result = get_args(&mut argc, &mut argv_buf);
    if result > 0 && argc > 1 {
        // 解析argv_buf中的参数
        let mut args = Vec::new();
        let mut offset = 0;

        for _ in 0..argc {
            if offset >= argv_buf.len() {
                break;
            }

            // 找到下一个null终止符
            let arg_end = argv_buf[offset..]
                .iter()
                .position(|&x| x == 0)
                .unwrap_or(argv_buf.len() - offset);

            if arg_end > 0 {
                if let Ok(arg_str) = core::str::from_utf8(&argv_buf[offset..offset + arg_end]) {
                    args.push(arg_str.to_string());
                }
            }

            offset += arg_end + 1;
        }

        // 第一个参数通常是程序名，第二个参数应该是WASM文件名
        if args.len() > 1 {
            let wasm_file = args[1].clone();
            let wasm_args = if args.len() > 2 {
                args[2..].to_vec()
            } else {
                vec![]
            };

            println!("Parsed {} arguments from system call:", argc);
            for (i, arg) in args.iter().enumerate() {
                println!("  argv[{}] = {}", i, arg);
            }

            return (wasm_file, wasm_args);
        }
    }

    println!(
        "Failed to get arguments via system call, argc={}, result={}",
        argc, result
    );

    (String::new(), vec![])
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
    if args.wasm_file.is_empty() {
        println!("Usage: wasm_runtime <wasm_file> [args...]");
        println!("Example: wasm_runtime hello.wasm arg1 arg2");
        return 1;
    }

    // 创建并启动WASM运行时
    let mut runtime = match WasmRuntimeService::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            println!("Failed to initialize WASM runtime: {}", e);
            return 1;
        }
    };

    // 执行WASM程序
    let exit_code = match runtime.execute_wasm(&args.wasm_file, &args.wasm_args, &args.env_vars) {
        Ok(exit_code) => {
            exit_code
        }
        Err(e) => {
            println!("WASM execution failed: {}", e);
            1
        }
    };

    exit_code
}
