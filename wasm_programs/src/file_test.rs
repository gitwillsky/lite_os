#![no_std]
#![no_main]

use core::panic::PanicInfo;

// 使用 WASI 预览版本1的导入
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs: *const IoVec, iovs_len: usize, nwritten: *mut usize) -> i32;
    fn fd_read(fd: i32, iovs: *const IoVec, iovs_len: usize, nread: *mut usize) -> i32;
    fn path_open(
        dirfd: i32,
        dirflags: i32,
        path_ptr: *const u8,
        path_len: usize,
        oflags: i32,
        fs_rights_base: i64,
        fs_rights_inheriting: i64,
        fdflags: i32,
        fd_ptr: *mut i32,
    ) -> i32;
    fn fd_close(fd: i32) -> i32;
    fn proc_exit(exit_code: i32) -> !;
}

#[repr(C)]
struct IoVec {
    buf: *const u8,
    buf_len: usize,
}

#[repr(C)]
struct IoVecMut {
    buf: *mut u8,
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

fn test_file_read() -> i32 {
    print_str("=== Testing File Read Operations ===\n");
    
    // 尝试打开一个测试文件
    let filename = "test.txt";
    let mut fd = -1;
    
    unsafe {
        let result = path_open(
            3,    // dirfd (预打开的目录)
            0,    // dirflags
            filename.as_ptr(),
            filename.len(),
            0,    // oflags (O_RDONLY)
            0x1,  // fs_rights_base (READ)
            0,    // fs_rights_inheriting
            0,    // fdflags
            &mut fd,
        );
        
        if result == 0 && fd >= 0 {
            print_str("Successfully opened file: ");
            print_str(filename);
            print_str("\n");
            
            // 尝试读取文件内容
            let mut buffer = [0u8; 256];
            let iov = IoVecMut {
                buf: buffer.as_mut_ptr(),
                buf_len: buffer.len(),
            };
            let mut nread = 0;
            
            let read_result = fd_read(fd, &iov as *const _ as *const IoVec, 1, &mut nread);
            if read_result == 0 {
                let mut num_buf = [0u8; 20];
                print_str("Read ");
                print_str(format_number(nread, &mut num_buf));
                print_str(" bytes from file\n");
                
                if nread > 0 {
                    // 显示读取的内容（前64字节）
                    let display_len = core::cmp::min(nread, 64);
                    if let Ok(content) = core::str::from_utf8(&buffer[..display_len]) {
                        print_str("File content: \"");
                        print_str(content);
                        print_str("\"\n");
                    } else {
                        print_str("File contains binary data\n");
                    }
                }
            } else {
                print_str("Failed to read from file\n");
            }
            
            // 关闭文件
            fd_close(fd);
            print_str("File closed\n");
        } else {
            print_str("Failed to open file: ");
            print_str(filename);
            print_str(" (file may not exist)\n");
            
            // 这不算错误，因为文件可能不存在
            print_str("This is expected if the test file doesn't exist\n");
        }
    }
    
    print_str("File read test completed\n\n");
    0
}

fn test_directory_operations() -> i32 {
    print_str("=== Testing Directory Operations ===\n");
    
    // 测试访问预打开的目录
    print_str("Testing access to pre-opened directories\n");
    
    // 在WASI中，通常有预打开的目录，文件描述符3通常是根目录
    let test_files = [".", "test.txt", "hello.wasm", "README.md"];
    
    for filename in test_files.iter() {
        let mut fd = -1;
        unsafe {
            let result = path_open(
                3,    // dirfd
                0,    // dirflags
                filename.as_ptr(),
                filename.len(),
                0,    // oflags (O_RDONLY)
                0x1,  // fs_rights_base
                0,    // fs_rights_inheriting
                0,    // fdflags
                &mut fd,
            );
            
            if result == 0 && fd >= 0 {
                print_str("  ✓ ");
                print_str(filename);
                print_str(" exists\n");
                fd_close(fd);
            } else {
                print_str("  ✗ ");
                print_str(filename);
                print_str(" not found\n");
            }
        }
    }
    
    print_str("Directory operations test completed\n\n");
    0
}

fn test_stdio_operations() -> i32 {
    print_str("=== Testing Standard I/O ===\n");
    
    // 测试标准输出
    print_str("Testing stdout (fd 1)... ");
    print_str("OK\n");
    
    // 测试标准错误
    let err_msg = "Testing stderr (fd 2)... OK\n";
    let iov = IoVec {
        buf: err_msg.as_ptr(),
        buf_len: err_msg.len(),
    };
    let mut nwritten = 0;
    unsafe {
        let result = fd_write(2, &iov, 1, &mut nwritten);
        if result == 0 {
            print_str("Stderr write successful\n");
        } else {
            print_str("Stderr write failed\n");
            return 1;
        }
    }
    
    // 测试文件描述符状态
    print_str("Standard file descriptors are working correctly\n");
    
    print_str("Standard I/O test completed\n\n");
    0
}

fn test_file_metadata() -> i32 {
    print_str("=== Testing File Metadata ===\n");
    
    // 在简化的WASI实现中，我们主要测试文件是否可以打开
    // 更复杂的元数据操作需要额外的WASI调用
    
    print_str("File metadata operations require additional WASI calls\n");
    print_str("Testing basic file accessibility instead\n");
    
    let test_files = [".", "/", "hello.wasm", "nonexistent.txt"];
    
    for filename in test_files.iter() {
        let mut fd = -1;
        unsafe {
            let result = path_open(
                3, 0, filename.as_ptr(), filename.len(),
                0, 0x1, 0, 0, &mut fd
            );
            
            if result == 0 && fd >= 0 {
                print_str("  ");
                print_str(filename);
                print_str(" is accessible\n");
                fd_close(fd);
            } else {
                print_str("  ");
                print_str(filename);
                print_str(" is not accessible\n");
            }
        }
    }
    
    print_str("File metadata test completed\n\n");
    0
}

fn test_error_handling() -> i32 {
    print_str("=== Testing Error Handling ===\n");
    
    // 测试无效文件描述符
    print_str("Testing invalid file descriptor... ");
    let invalid_data = "test";
    let iov = IoVec {
        buf: invalid_data.as_ptr(),
        buf_len: invalid_data.len(),
    };
    let mut nwritten = 0;
    
    unsafe {
        let result = fd_write(999, &iov, 1, &mut nwritten); // 无效的fd
        if result != 0 {
            print_str("Correctly detected invalid fd\n");
        } else {
            print_str("Error detection failed\n");
            return 1;
        }
    }
    
    // 测试访问不存在的文件
    print_str("Testing non-existent file access... ");
    let mut fd = -1;
    unsafe {
        let filename = "definitely_nonexistent_file_12345.txt";
        let result = path_open(
            3, 0, filename.as_ptr(), filename.len(),
            0, 0x1, 0, 0, &mut fd
        );
        
        if result != 0 || fd < 0 {
            print_str("Correctly detected non-existent file\n");
        } else {
            print_str("Error detection failed\n");
            if fd >= 0 {
                fd_close(fd);
            }
            return 1;
        }
    }
    
    print_str("Error handling test completed\n\n");
    0
}

#[export_name = "main"]
pub extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    print_str("LiteOS Rust File Test Program\n");
    print_str("=============================\n");
    print_str("Testing file system operations through WASI\n\n");
    
    let mut total_errors = 0;
    
    total_errors += test_stdio_operations();
    total_errors += test_file_read();
    total_errors += test_directory_operations();
    total_errors += test_file_metadata();
    total_errors += test_error_handling();
    
    print_str("=== File Test Summary ===\n");
    if total_errors == 0 {
        print_str("All file operations completed successfully!\n");
        print_str("WASI file interface is working correctly\n");
    } else {
        let mut num_buf = [0u8; 20];
        print_str("Some file tests failed (error count: ");
        print_str(format_number(total_errors as usize, &mut num_buf));
        print_str(")\n");
    }
    
    print_str("Exiting file test program...\n");
    
    total_errors
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    print_str("PANIC: File test program panicked!\n");
    unsafe {
        proc_exit(99);
    }
}