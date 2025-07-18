#![no_std]
#![no_main]

use core::panic::PanicInfo;

// 使用 WASI 预览版本1的导入
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs: *const IoVec, iovs_len: usize, nwritten: *mut usize) -> i32;
    fn fd_read(fd: i32, iovs: *const IoVec, iovs_len: usize, nread: *mut usize) -> i32;
    fn proc_exit(exit_code: i32) -> !;
    fn args_sizes_get(argc_ptr: *mut usize, argv_buf_size_ptr: *mut usize) -> i32;
    fn args_get(argv_ptr: *mut *mut u8, argv_buf_ptr: *mut u8) -> i32;
    fn environ_sizes_get(environc_ptr: *mut usize, environ_buf_size_ptr: *mut usize) -> i32;
    fn environ_get(environ_ptr: *mut *mut u8, environ_buf_ptr: *mut u8) -> i32;
    fn sched_yield() -> i32;
}

#[repr(C)]
struct IoVec {
    buf: *const u8,
    buf_len: usize,
}

// 打印函数
fn print_str(s: &str) {
    let iov = IoVec {
        buf: s.as_ptr(),
        buf_len: s.len(),
    };
    let mut nwritten = 0;
    unsafe {
        fd_write(1, &iov, 1, &mut nwritten);
    }
}

fn print_err(s: &str) {
    let iov = IoVec {
        buf: s.as_ptr(),
        buf_len: s.len(),
    };
    let mut nwritten = 0;
    unsafe {
        fd_write(2, &iov, 1, &mut nwritten);
    }
}

// 数字格式化
fn format_number(mut num: usize, buf: &mut [u8]) -> &str {
    if num == 0 {
        buf[0] = b'0';
        return core::str::from_utf8(&buf[0..1]).unwrap();
    }
    
    let mut pos = buf.len();
    while num > 0 {
        pos -= 1;
        buf[pos] = b'0' + (num % 10) as u8;
        num /= 10;
    }
    core::str::from_utf8(&buf[pos..]).unwrap()
}

fn test_basic_io() -> i32 {
    print_str("=== Testing Basic I/O ===\n");
    
    // 测试 stdout
    print_str("Testing stdout output... ");
    print_str("OK\n");
    
    // 测试 stderr
    print_err("Testing stderr output... ");
    print_err("OK\n");
    
    print_str("Basic I/O test completed\n\n");
    0
}

fn test_arguments() -> i32 {
    print_str("=== Testing Arguments ===\n");
    
    let mut argc = 0;
    let mut argv_buf_size = 0;
    let mut num_buf = [0u8; 20];
    
    unsafe {
        let result = args_sizes_get(&mut argc, &mut argv_buf_size);
        if result == 0 {
            print_str("Arguments count: ");
            print_str(format_number(argc, &mut num_buf));
            print_str("\n");
            
            print_str("Arguments buffer size: ");
            print_str(format_number(argv_buf_size, &mut num_buf));
            print_str(" bytes\n");
            
            if argc > 0 && argv_buf_size > 0 {
                // 这里应该分配内存获取实际参数，但在 no_std 环境中简化处理
                print_str("Arguments available (detailed parsing not implemented in no_std)\n");
            }
        } else {
            print_str("Failed to get arguments information\n");
            return 1;
        }
    }
    
    print_str("Arguments test completed\n\n");
    0
}

fn test_environment() -> i32 {
    print_str("=== Testing Environment ===\n");
    
    let mut environc = 0;
    let mut environ_buf_size = 0;
    let mut num_buf = [0u8; 20];
    
    unsafe {
        let result = environ_sizes_get(&mut environc, &mut environ_buf_size);
        if result == 0 {
            print_str("Environment variables count: ");
            print_str(format_number(environc, &mut num_buf));
            print_str("\n");
            
            print_str("Environment buffer size: ");
            print_str(format_number(environ_buf_size, &mut num_buf));
            print_str(" bytes\n");
            
            if environc > 0 {
                print_str("Environment variables available\n");
            } else {
                print_str("No environment variables found\n");
            }
        } else {
            print_str("Failed to get environment information\n");
            return 1;
        }
    }
    
    print_str("Environment test completed\n\n");
    0
}

fn test_process_control() -> i32 {
    print_str("=== Testing Process Control ===\n");
    
    // 测试 sched_yield
    print_str("Testing sched_yield... ");
    unsafe {
        let result = sched_yield();
        if result == 0 {
            print_str("OK\n");
        } else {
            print_str("FAILED\n");
            return 1;
        }
    }
    
    print_str("Process control test completed\n\n");
    0
}

fn test_memory_operations() -> i32 {
    print_str("=== Testing Memory Operations ===\n");
    
    // 测试栈内存
    let mut buffer = [0u8; 256];
    let test_data = b"Hello, WASM Memory!";
    
    // 复制数据到缓冲区
    for (i, &byte) in test_data.iter().enumerate() {
        if i < buffer.len() {
            buffer[i] = byte;
        }
    }
    
    print_str("Memory copy test: ");
    let copied_str = core::str::from_utf8(&buffer[..test_data.len()]).unwrap();
    print_str(copied_str);
    print_str("\n");
    
    // 测试数组操作
    let numbers = [1, 2, 3, 4, 5];
    let mut sum = 0;
    for &num in numbers.iter() {
        sum += num;
    }
    
    let mut num_buf = [0u8; 20];
    print_str("Array sum test: ");
    print_str(format_number(sum, &mut num_buf));
    print_str("\n");
    
    print_str("Memory operations test completed\n\n");
    0
}

fn test_control_flow() -> i32 {
    print_str("=== Testing Control Flow ===\n");
    
    // 测试循环
    print_str("Loop test: counting from 1 to 5\n");
    let mut num_buf = [0u8; 20];
    for i in 1..=5 {
        print_str("  ");
        print_str(format_number(i, &mut num_buf));
        print_str("\n");
    }
    
    // 测试条件判断
    let value = 42;
    if value > 40 {
        print_str("Conditional test: value is greater than 40\n");
    } else {
        print_str("Conditional test: value is not greater than 40\n");
    }
    
    // 测试 match
    let result_msg = match value % 3 {
        0 => "divisible by 3",
        1 => "remainder 1 when divided by 3",
        2 => "remainder 2 when divided by 3",
        _ => "unexpected remainder",
    };
    print_str("Match test: ");
    print_str(result_msg);
    print_str("\n");
    
    print_str("Control flow test completed\n\n");
    0
}

#[export_name = "main"]
pub extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    print_str("LiteOS Rust WASI Test Program\n");
    print_str("=============================\n");
    print_str("Comprehensive WASI functionality testing\n\n");
    
    let mut total_errors = 0;
    
    total_errors += test_basic_io();
    total_errors += test_arguments();
    total_errors += test_environment();
    total_errors += test_process_control();
    total_errors += test_memory_operations();
    total_errors += test_control_flow();
    
    print_str("=== Test Summary ===\n");
    if total_errors == 0 {
        print_str("All tests PASSED!\n");
        print_str("WASI interface is working correctly\n");
    } else {
        let mut num_buf = [0u8; 20];
        print_str("Some tests FAILED (error count: ");
        print_str(format_number(total_errors as usize, &mut num_buf));
        print_str(")\n");
    }
    
    print_str("Exiting WASI test program...\n");
    
    total_errors
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    print_err("PANIC: WASI test program panicked!\n");
    unsafe {
        proc_exit(99);
    }
}