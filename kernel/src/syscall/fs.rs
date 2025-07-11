use alloc::string::String;

use crate::{
    arch::sbi,
    fs::vfs::get_vfs,
    memory::page_table::translated_byte_buffer,
    task::{current_user_token, suspend_current_and_run_next},
};

const STD_OUT: usize = 1;
const STD_IN: usize = 0;

/// write buf of length `len`  to a file with `fd`
pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    match fd {
        STD_OUT => {
            let buffers = translated_byte_buffer(current_user_token(), buf, len);
            for buffer in buffers {
                let s = core::str::from_utf8(buffer).unwrap();
                for c in s.bytes() {
                    sbi::console_putchar(c as usize);
                }
            }
            len as isize
        }
        _ => {
            // 支持真实文件写入
            // 这里可以扩展为支持文件描述符
            -1
        }
    }
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    match fd {
        STD_IN => {
            if len == 0 {
                return 0;
            }
            assert_eq!(len, 1, "Only support len = 1 in sys_read!");
            let buffers = translated_byte_buffer(current_user_token(), buf, len);
            let ch = loop {
                let c = sbi::console_getchar();
                if c == -1 {
                    suspend_current_and_run_next();
                    continue;
                } else {
                    break c;
                }
            };
            let user_buf = buffers.into_iter().next().unwrap();
            if !user_buf.is_empty() {
                user_buf[0] = ch as u8;
                1
            } else {
                0
            }
        }
        _ => {
            // 支持真实文件读取
            // 这里可以扩展为支持文件描述符
            -1
        }
    }
}

/// 打开文件
pub fn sys_open(path: *const u8, flags: u32) -> isize {
    let token = current_user_token();
    let path_str = translated_c_string(token, path);
    
    match get_vfs().open(&path_str) {
        Ok(_inode) => {
            // 返回文件描述符
            // 这里需要进程文件描述符表的支持
            3 // 暂时返回固定值
        }
        Err(_) => -1,
    }
}

/// 关闭文件
pub fn sys_close(fd: usize) -> isize {
    match fd {
        STD_IN | STD_OUT => 0,
        _ => {
            // 关闭文件描述符
            0
        }
    }
}

/// 列出目录
pub fn sys_listdir(path: *const u8, buf: *mut u8, len: usize) -> isize {
    let token = current_user_token();
    let path_str = translated_c_string(token, path);
    
    match get_vfs().open(&path_str) {
        Ok(inode) => {
            match inode.list_dir() {
                Ok(entries) => {
                    let mut result = String::new();
                    for entry in entries {
                        result.push_str(&entry);
                        result.push('\n');
                    }
                    
                    let result_bytes = result.as_bytes();
                    let copy_len = result_bytes.len().min(len);
                    
                    let buffers = translated_byte_buffer(token, buf, copy_len);
                    let mut offset = 0;
                    for buffer in buffers {
                        let chunk_len = buffer.len().min(result_bytes.len() - offset);
                        buffer[..chunk_len].copy_from_slice(&result_bytes[offset..offset + chunk_len]);
                        offset += chunk_len;
                        if offset >= result_bytes.len() {
                            break;
                        }
                    }
                    
                    copy_len as isize
                }
                Err(_) => -1,
            }
        }
        Err(_) => -1,
    }
}

/// 创建目录
pub fn sys_mkdir(path: *const u8) -> isize {
    let token = current_user_token();
    let path_str = translated_c_string(token, path);
    
    match get_vfs().create_directory(&path_str) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

/// 删除文件或目录
pub fn sys_remove(path: *const u8) -> isize {
    let token = current_user_token();
    let path_str = translated_c_string(token, path);
    
    match get_vfs().remove(&path_str) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

/// 获取文件信息
pub fn sys_stat(path: *const u8, stat_buf: *mut u8) -> isize {
    let token = current_user_token();
    let path_str = translated_c_string(token, path);
    
    match get_vfs().open(&path_str) {
        Ok(inode) => {
            // 这里需要根据实际的stat结构体来填充
            // 暂时返回成功
            0
        }
        Err(_) => -1,
    }
}

/// 读取文件内容
pub fn sys_read_file(path: *const u8, buf: *mut u8, len: usize) -> isize {
    let token = current_user_token();
    let path_str = translated_c_string(token, path);
    
    match get_vfs().open(&path_str) {
        Ok(inode) => {
            let buffers = translated_byte_buffer(token, buf, len);
            let mut total_read = 0;
            let mut offset = 0;
            
            for buffer in buffers {
                match inode.read_at(offset, buffer) {
                    Ok(bytes_read) => {
                        total_read += bytes_read;
                        offset += bytes_read as u64;
                        if bytes_read < buffer.len() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            
            total_read as isize
        }
        Err(_) => -1,
    }
}

// 辅助函数：将C字符串转换为Rust字符串
fn translated_c_string(token: usize, ptr: *const u8) -> String {
    let mut string = String::new();
    let mut va = ptr as usize;
    
    loop {
        let buffers = translated_byte_buffer(token, va as *const u8, 1);
        if buffers.is_empty() {
            break;
        }
        let ch = buffers[0][0];
        if ch == 0 {
            break;
        }
        string.push(ch as char);
        va += 1;
    }
    
    string
}
