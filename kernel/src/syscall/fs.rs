use alloc::string::String;
use alloc::format;

use crate::{
    arch::sbi,
    fs::{vfs::get_vfs, FileSystemError},
    memory::page_table::translated_byte_buffer,
    task::{current_user_token, suspend_current_and_run_next, current_task},
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
        Err(e) => {
            // Return more specific error codes
            match e {
                FileSystemError::AlreadyExists => -17, // EEXIST
                FileSystemError::PermissionDenied => -13, // EACCES
                FileSystemError::NotFound => -2, // ENOENT (parent directory not found)
                FileSystemError::NotDirectory => -20, // ENOTDIR
                FileSystemError::NoSpace => -28, // ENOSPC
                _ => -1, // Generic error
            }
        }
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
            // Read the entire file into a temporary buffer first
            let file_size = inode.size() as usize;
            let read_size = file_size.min(len);
            
            if read_size == 0 {
                return 0;
            }
            
            // Create a temporary buffer to read the file contents
            let mut temp_buf = alloc::vec![0u8; read_size];
            
            match inode.read_at(0, &mut temp_buf) {
                Ok(bytes_read) => {
                    // Now copy to user space using translated_byte_buffer
                    let buffers = translated_byte_buffer(token, buf, bytes_read);
                    let mut offset = 0;
                    
                    for buffer in buffers {
                        if offset >= bytes_read {
                            break;
                        }
                        let copy_len = buffer.len().min(bytes_read - offset);
                        buffer[..copy_len].copy_from_slice(&temp_buf[offset..offset + copy_len]);
                        offset += copy_len;
                    }
                    
                    bytes_read as isize
                }
                Err(_) => -1
            }
        }
        Err(_) => -1
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

/// 改变当前工作目录
pub fn sys_chdir(path: *const u8) -> isize {
    let token = current_user_token();
    let path_str = translated_c_string(token, path);
    
    // Check if the path exists and is a directory
    match get_vfs().open(&path_str) {
        Ok(inode) => {
            // Try to list the directory to verify it's actually a directory
            match inode.list_dir() {
                Ok(_) => {
                    // It's a directory, set the current working directory for the current task
                    if let Some(task) = current_task() {
                        let mut task_inner = task.inner_exclusive_access();
                        // Use VFS to resolve the path properly
                        let absolute_path = get_vfs().resolve_relative_path(&path_str);
                        task_inner.cwd = absolute_path;
                        0 // Success
                    } else {
                        -1 // No current task
                    }
                }
                Err(FileSystemError::NotDirectory) => -20, // ENOTDIR - Not a directory
                Err(_) => -13, // EACCES - Permission denied or other error
            }
        }
        Err(e) => {
            match e {
                FileSystemError::NotFound => -2, // ENOENT
                FileSystemError::PermissionDenied => -13, // EACCES
                _ => -1, // Generic error
            }
        }
    }
}

/// 获取当前工作目录
pub fn sys_getcwd(buf: *mut u8, len: usize) -> isize {
    let token = current_user_token();
    
    if let Some(task) = current_task() {
        let task_inner = task.inner_exclusive_access();
        let cwd_bytes = task_inner.cwd.as_bytes();
        let copy_len = (cwd_bytes.len() + 1).min(len); // +1 for null terminator
        
        if copy_len == 0 {
            return -22; // EINVAL - Buffer too small
        }
        
        let buffers = translated_byte_buffer(token, buf, copy_len);
        let mut offset = 0;
        
        for buffer in buffers {
            if offset >= cwd_bytes.len() {
                break;
            }
            let chunk_len = buffer.len().min(cwd_bytes.len() - offset);
            buffer[..chunk_len].copy_from_slice(&cwd_bytes[offset..offset + chunk_len]);
            offset += chunk_len;
        }
        
        // Add null terminator if there's space
        if offset < copy_len {
            let mut buffers = translated_byte_buffer(token, (buf as usize + offset) as *mut u8, 1);
            if !buffers.is_empty() && !buffers[0].is_empty() {
                buffers[0][0] = 0;
            }
        }
        
        copy_len as isize
    } else {
        -1 // No current task
    }
}
