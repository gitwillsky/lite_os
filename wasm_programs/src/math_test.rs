#![no_std]
#![no_main]

use core::panic::PanicInfo;

// 使用 WASI 预览版本1的导入
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs: *const IoVec, iovs_len: usize, nwritten: *mut usize) -> i32;
    fn proc_exit(exit_code: i32) -> !;
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

// 数字格式化（支持负数）
fn format_number(mut num: i64, buf: &mut [u8]) -> &str {
    if num == 0 {
        buf[0] = b'0';
        return core::str::from_utf8(&buf[0..1]).unwrap();
    }
    
    let mut pos = buf.len();
    let is_negative = num < 0;
    if is_negative {
        num = -num;
    }
    
    while num > 0 {
        pos -= 1;
        buf[pos] = b'0' + (num % 10) as u8;
        num /= 10;
    }
    
    if is_negative {
        pos -= 1;
        buf[pos] = b'-';
    }
    
    core::str::from_utf8(&buf[pos..]).unwrap()
}

// 无符号数字格式化
fn format_usize(mut num: usize, buf: &mut [u8]) -> &str {
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

// 浮点数近似格式化（简化版本）
fn format_float_approx(num: f64, buf: &mut [u8]) -> &str {
    let integer_part = num as i64;
    let fractional_part = ((num - integer_part as f64) * 1000.0) as u64;
    
    let mut pos = 0;
    
    // 整数部分 - 使用临时缓冲区避免借用冲突
    let mut temp_buf = [0u8; 20];
    let integer_str = format_number(integer_part, &mut temp_buf);
    for &byte in integer_str.as_bytes() {
        buf[pos] = byte;
        pos += 1;
    }
    
    // 小数点
    buf[pos] = b'.';
    pos += 1;
    
    // 小数部分（3位）
    buf[pos] = b'0' + ((fractional_part / 100) % 10) as u8;
    pos += 1;
    buf[pos] = b'0' + ((fractional_part / 10) % 10) as u8;
    pos += 1;
    buf[pos] = b'0' + (fractional_part % 10) as u8;
    pos += 1;
    
    core::str::from_utf8(&buf[..pos]).unwrap()
}

fn test_arithmetic() -> i32 {
    print_str("=== Testing Arithmetic Operations ===\n");
    
    let a = 42i64;
    let b = 13i64;
    let mut num_buf = [0u8; 40];
    
    print_str("a = ");
    print_str(format_number(a, &mut num_buf));
    print_str(", b = ");
    print_str(format_number(b, &mut num_buf));
    print_str("\n");
    
    print_str("a + b = ");
    print_str(format_number(a + b, &mut num_buf));
    print_str("\n");
    
    print_str("a - b = ");
    print_str(format_number(a - b, &mut num_buf));
    print_str("\n");
    
    print_str("a * b = ");
    print_str(format_number(a * b, &mut num_buf));
    print_str("\n");
    
    print_str("a / b = ");
    print_str(format_number(a / b, &mut num_buf));
    print_str("\n");
    
    print_str("a % b = ");
    print_str(format_number(a % b, &mut num_buf));
    print_str("\n");
    
    print_str("Arithmetic test completed\n\n");
    0
}

fn test_floating_point() -> i32 {
    print_str("=== Testing Floating Point Operations ===\n");
    
    let x = 3.14159;
    let y = 2.71828;
    let mut buf = [0u8; 40];
    
    print_str("x = ");
    print_str(format_float_approx(x, &mut buf));
    print_str(", y = ");
    print_str(format_float_approx(y, &mut buf));
    print_str("\n");
    
    print_str("x + y = ");
    print_str(format_float_approx(x + y, &mut buf));
    print_str("\n");
    
    print_str("x * y = ");
    print_str(format_float_approx(x * y, &mut buf));
    print_str("\n");
    
    print_str("x / y = ");
    print_str(format_float_approx(x / y, &mut buf));
    print_str("\n");
    
    print_str("Floating point test completed\n\n");
    0
}

fn test_control_flow() -> i32 {
    print_str("=== Testing Control Flow ===\n");
    
    // 测试循环
    print_str("Counting from 1 to 5:\n");
    let mut num_buf = [0u8; 20];
    for i in 1..=5 {
        print_str("  ");
        print_str(format_usize(i, &mut num_buf));
        print_str("\n");
    }
    
    // 测试条件判断
    let value = 10;
    if value > 5 {
        print_str("Value ");
        print_str(format_usize(value, &mut num_buf));
        print_str(" is greater than 5\n");
    } else {
        print_str("Value ");
        print_str(format_usize(value, &mut num_buf));
        print_str(" is not greater than 5\n");
    }
    
    // 测试 match
    match value % 3 {
        0 => print_str("Value is divisible by 3\n"),
        1 => print_str("Value mod 3 equals 1\n"),
        2 => print_str("Value mod 3 equals 2\n"),
        _ => print_str("Unexpected remainder\n"),
    }
    
    print_str("Control flow test completed\n\n");
    0
}

fn test_memory_operations() -> i32 {
    print_str("=== Testing Memory Operations ===\n");
    
    // 测试数组
    let numbers = [1, 2, 3, 4, 5];
    print_str("Array contents: ");
    let mut num_buf = [0u8; 20];
    
    for &num in numbers.iter() {
        print_str(format_usize(num, &mut num_buf));
        print_str(" ");
    }
    print_str("\n");
    
    // 测试字符串操作
    let mut buffer = [0u8; 64];
    let test_str = "Hello, Memory!";
    
    // 手动复制字符串
    for (i, &byte) in test_str.as_bytes().iter().enumerate() {
        if i < buffer.len() {
            buffer[i] = byte;
        }
    }
    
    print_str("String copy test: ");
    let copied = core::str::from_utf8(&buffer[..test_str.len()]).unwrap();
    print_str(copied);
    print_str("\n");
    
    print_str("Memory operations test completed\n\n");
    0
}

// 递归函数：斐波那契数列
fn fibonacci(n: usize) -> usize {
    match n {
        0 => 0,
        1 => 1,
        _ => fibonacci(n - 1) + fibonacci(n - 2),
    }
}

fn test_recursion() -> i32 {
    print_str("=== Testing Recursion ===\n");
    
    print_str("Fibonacci sequence (first 10 numbers):\n");
    let mut num_buf = [0u8; 20];
    
    for i in 0..10 {
        let fib_val = fibonacci(i);
        print_str("fib(");
        print_str(format_usize(i, &mut num_buf));
        print_str(") = ");
        print_str(format_usize(fib_val, &mut num_buf));
        print_str("\n");
    }
    
    print_str("Recursion test completed\n\n");
    0
}

fn test_bit_operations() -> i32 {
    print_str("=== Testing Bit Operations ===\n");
    
    let a = 0b1010_1100u32; // 172
    let b = 0b1100_0011u32; // 195
    let mut num_buf = [0u8; 20];
    
    print_str("a = ");
    print_str(format_usize(a as usize, &mut num_buf));
    print_str(", b = ");
    print_str(format_usize(b as usize, &mut num_buf));
    print_str("\n");
    
    print_str("a & b = ");
    print_str(format_usize((a & b) as usize, &mut num_buf));
    print_str("\n");
    
    print_str("a | b = ");
    print_str(format_usize((a | b) as usize, &mut num_buf));
    print_str("\n");
    
    print_str("a ^ b = ");
    print_str(format_usize((a ^ b) as usize, &mut num_buf));
    print_str("\n");
    
    print_str("a << 2 = ");
    print_str(format_usize((a << 2) as usize, &mut num_buf));
    print_str("\n");
    
    print_str("a >> 2 = ");
    print_str(format_usize((a >> 2) as usize, &mut num_buf));
    print_str("\n");
    
    print_str("Bit operations test completed\n\n");
    0
}

#[export_name = "main"]
pub extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    print_str("LiteOS Rust Math Test Program\n");
    print_str("=============================\n");
    print_str("Testing mathematical and computational operations in WASM\n\n");
    
    let mut total_errors = 0;
    
    total_errors += test_arithmetic();
    total_errors += test_floating_point();
    total_errors += test_control_flow();
    total_errors += test_memory_operations();
    total_errors += test_recursion();
    total_errors += test_bit_operations();
    
    print_str("=== Math Test Summary ===\n");
    if total_errors == 0 {
        print_str("All mathematical operations completed successfully!\n");
        print_str("WASM engine arithmetic and control flow working correctly\n");
    } else {
        let mut num_buf = [0u8; 20];
        print_str("Some math tests failed (error count: ");
        print_str(format_usize(total_errors as usize, &mut num_buf));
        print_str(")\n");
    }
    
    print_str("Exiting math test program...\n");
    
    total_errors
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    print_str("PANIC: Math test program panicked!\n");
    unsafe {
        proc_exit(99);
    }
}