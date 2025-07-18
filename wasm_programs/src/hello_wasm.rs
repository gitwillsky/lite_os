#![no_std]
#![no_main]

// 使用 WASI 预览版本2的导入
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs: *const IoVec, iovs_len: usize, nwritten: *mut usize) -> i32;
    fn proc_exit(exit_code: i32) -> !;
    fn args_sizes_get(argc_ptr: *mut usize, argv_buf_size_ptr: *mut usize) -> i32;
    fn args_get(argv_ptr: *mut *mut u8, argv_buf_ptr: *mut u8) -> i32;
}

use core::panic::PanicInfo;


#[repr(C)]
struct IoVec {
    buf: *const u8,
    buf_len: usize,
}

// 简单的字符串打印函数
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

// 格式化数字为字符串（简化版本）
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

#[export_name = "main"]
pub extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    print_str("Hello from Rust WASM!\n");
    print_str("=======================\n");
    print_str("This is a Rust-based WASM test program for LiteOS\n\n");
    
    // 获取命令行参数信息
    let mut argc = 0;
    let mut argv_buf_size = 0;
    
    unsafe {
        let result = args_sizes_get(&mut argc, &mut argv_buf_size);
        if result == 0 {
            let mut num_buf = [0u8; 20];
            print_str("Arguments count: ");
            print_str(format_number(argc, &mut num_buf));
            print_str("\n");
            
            print_str("Arguments buffer size: ");
            print_str(format_number(argv_buf_size, &mut num_buf));
            print_str(" bytes\n");
        } else {
            print_str("Failed to get arguments information\n");
        }
    }
    
    print_str("\nTesting basic Rust features:\n");
    
    // 测试基本运算
    let a = 42;
    let b = 13;
    let result = a + b;
    let mut num_buf = [0u8; 20];
    
    print_str("Arithmetic test: ");
    print_str(format_number(a, &mut num_buf));
    print_str(" + ");
    print_str(format_number(b, &mut num_buf));
    print_str(" = ");
    print_str(format_number(result, &mut num_buf));
    print_str("\n");
    
    // 测试数组和循环
    print_str("\nArray and loop test:\n");
    let numbers = [1, 2, 3, 4, 5];
    for (i, &num) in numbers.iter().enumerate() {
        print_str("numbers[");
        print_str(format_number(i, &mut num_buf));
        print_str("] = ");
        print_str(format_number(num, &mut num_buf));
        print_str("\n");
    }
    
    // 测试字符串处理
    print_str("\nString handling test:\n");
    let test_string = "LiteOS WASM Runtime";
    print_str("Test string: \"");
    print_str(test_string);
    print_str("\"\n");
    print_str("String length: ");
    print_str(format_number(test_string.len(), &mut num_buf));
    print_str(" bytes\n");
    
    print_str("\nRust WASM test completed successfully!\n");
    print_str("Exiting with code 0...\n");
    
    0
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    print_str("PANIC: Rust WASM program panicked!\n");
    unsafe {
        proc_exit(1);
    }
}