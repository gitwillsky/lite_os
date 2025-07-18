use core::sync::atomic;

use alloc::{string::String, sync::Arc, vec::Vec};

use crate::{
    arch::sbi,
    fs::{vfs::get_vfs, FileSystemError, LockType, LockOp, LockError, get_file_lock_manager},
    memory::page_table::translated_byte_buffer,
    task::{current_user_token, suspend_current_and_run_next, current_task, FileDescriptor},
    ipc::{create_pipe, create_fifo},
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
            if let Some(task) = current_task() {
                // Get file descriptor while holding the lock briefly
                let file_desc = {
                    let task_inner = task.inner_exclusive_access();
                    task_inner.get_fd(fd)
                };
                
                if let Some(file_desc) = file_desc {
                    let buffers = translated_byte_buffer(current_user_token(), buf, len);
                    let mut data = Vec::new();
                    for buffer in buffers {
                        data.extend_from_slice(buffer);
                    }

                    // Call write_at without holding any locks on the task
                    match file_desc.write_at(&data) {
                        Ok(bytes_written) => {
                            bytes_written as isize
                        }
                        Err(_) => -1,
                    }
                } else {
                    -9 // EBADF - Bad file descriptor
                }
            } else {
                -1
            }
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
            if let Some(task) = current_task() {
                // Get file descriptor while holding the lock briefly
                let file_desc = {
                    let task_inner = task.inner_exclusive_access();
                    task_inner.get_fd(fd)
                };
                
                if let Some(file_desc) = file_desc {
                    let mut temp_buf = alloc::vec![0u8; len];
                    // Call read_at without holding any locks on the task
                    match file_desc.read_at(&mut temp_buf) {
                        Ok(bytes_read) => {
                            let buffers = translated_byte_buffer(current_user_token(), buf, bytes_read);
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
                        Err(_) => -1,
                    }
                } else {
                    -9 // EBADF - Bad file descriptor
                }
            } else {
                -1
            }
        }
    }
}

/// 打开文件
pub fn sys_open(path: *const u8, flags: u32) -> isize {
    let token = current_user_token();
    let path_str = translated_c_string(token, path);

    // Open flags
    const O_RDONLY: u32 = 0o0;
    const O_WRONLY: u32 = 0o1;
    const O_RDWR: u32 = 0o2;
    const O_CREAT: u32 = 0o100;
    const O_TRUNC: u32 = 0o1000;
    
    // 提取访问模式和文件标志
    let access_mode = flags & 0o3;
    let file_mode = flags & 0o777;   // 权限位（用于创建文件时）
    let has_creat = (flags & O_CREAT) != 0;
    let has_trunc = (flags & O_TRUNC) != 0;
    
    // 尝试打开现有文件
    let inode_result = get_vfs().open(&path_str);
    
    let inode = match inode_result {
        Ok(inode) => {
            // 文件存在，检查 O_TRUNC 标志
            if has_trunc {
                if let Err(_) = inode.truncate(0) {
                    return -1; // 截断失败
                }
            }
            inode
        }
        Err(_) => {
            // 文件不存在，检查 O_CREAT 标志
            if !has_creat {
                return -2; // ENOENT - 文件不存在且没有创建标志
            }
            
            // 创建新文件
            match get_vfs().create_file(&path_str) {
                Ok(inode) => {
                    debug!("[sys_open] File created successfully: {}", path_str);
                    // 设置文件权限（如果文件系统支持的话）
                    let _ = inode.set_mode(file_mode);
                    if let Some(task) = current_task() {
                        let task_inner = task.inner_exclusive_access();
                        let _ = inode.set_uid(task_inner.get_euid());
                        let _ = inode.set_gid(task_inner.get_egid());
                    }
                    inode
                }
                Err(e) => {
                    debug!("[sys_open] Failed to create file {}: {:?}", path_str, e);
                    return -1; // 创建失败
                }
            }
        }
    };

    if let Some(task) = current_task() {
        let task_inner = task.inner_exclusive_access();
        
        // 检查文件权限
        let file_mode = inode.get_mode();
        let file_uid = inode.get_uid();
        let file_gid = inode.get_gid();
        
        // 根据访问模式确定需要的权限
        let mut required_perm = 0;
        match access_mode {
            O_RDONLY => required_perm = 0o4, // 读权限
            O_WRONLY => required_perm = 0o2, // 写权限
            O_RDWR => required_perm = 0o6,   // 读写权限
            _ => required_perm = 0o4,        // 默认读权限
        }
        
        // 检查权限
        if !task_inner.check_file_permission(file_mode, file_uid, file_gid, required_perm) {
            return -13; // EACCES
        }
        
        drop(task_inner); // 释放锁
        
        let mut task_inner = task.inner_exclusive_access();
        let file_desc = Arc::new(FileDescriptor::new(inode, flags));
        let fd = task_inner.alloc_fd(file_desc);
        fd as isize
    } else {
        -1
    }
}

/// 关闭文件
pub fn sys_close(fd: usize) -> isize {
    match fd {
        STD_IN | STD_OUT => 0, // 标准流不需要关闭
        _ => {
            if let Some(task) = current_task() {
                let mut task_inner = task.inner_exclusive_access();
                if task_inner.close_fd(fd) {
                    0 // 成功关闭
                } else {
                    -9 // EBADF - Bad file descriptor
                }
            } else {
                -1
            }
        }
    }
}

/// 列出目录
pub fn sys_listdir(path: *const u8, buf: *mut u8, len: usize) -> isize {
    // 验证输入参数
    if buf.is_null() || len == 0 {
        return -14; // EFAULT
    }
    
    // 限制目录列表的最大长度防止内存溢出
    const MAX_DIR_LIST_SIZE: usize = 64 * 1024; // 64KB
    if len > MAX_DIR_LIST_SIZE {
        return -22; // EINVAL
    }

    let token = current_user_token();
    let path_str = translated_c_string(token, path);

    // 验证路径长度
    if path_str.len() > 4096 {
        return -36; // ENAMETOOLONG
    }

    match get_vfs().open(&path_str) {
        Ok(inode) => {
            match inode.list_dir() {
                Ok(entries) => {
                    let mut result = String::new();
                    let mut total_size = 0;
                    
                    // 限制条目数量和大小
                    for (i, entry) in entries.iter().enumerate() {
                        if i >= 1000 {  // 最多1000个条目
                            warn!("Directory listing truncated at 1000 entries");
                            break;
                        }
                        
                        let entry_with_newline = entry.len() + 1;
                        if total_size + entry_with_newline > MAX_DIR_LIST_SIZE {
                            warn!("Directory listing truncated due to size limit");
                            break;
                        }
                        
                        result.push_str(entry);
                        result.push('\n');
                        total_size += entry_with_newline;
                    }

                    let result_bytes = result.as_bytes();
                    let copy_len = result_bytes.len().min(len);

                    if copy_len > 0 {
                        let buffers = translated_byte_buffer(token, buf, copy_len);
                        let mut offset = 0;
                        for buffer in buffers {
                            if offset >= result_bytes.len() {
                                break;
                            }
                            let chunk_len = buffer.len().min(result_bytes.len() - offset);
                            if chunk_len > 0 {
                                buffer[..chunk_len].copy_from_slice(&result_bytes[offset..offset + chunk_len]);
                                offset += chunk_len;
                            }
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

/// 创建管道
pub fn sys_pipe(pipefd: *mut i32) -> isize {
    if let Some(task) = current_task() {
        let mut task_inner = task.inner_exclusive_access();
        let token = task_inner.get_user_token();

        // 创建管道
        let (read_end, write_end) = create_pipe();

        // 创建文件描述符
        let read_fd_desc = Arc::new(FileDescriptor::new(read_end, 0));
        let write_fd_desc = Arc::new(FileDescriptor::new(write_end, 0));

        // 分配文件描述符
        let read_fd = task_inner.alloc_fd(read_fd_desc);
        let write_fd = task_inner.alloc_fd(write_fd_desc);

        // 将文件描述符写入用户空间
        let mut buffers = translated_byte_buffer(token, pipefd as *const u8, 8); // 2 * sizeof(i32)
        if buffers.len() >= 1 && buffers[0].len() >= 8 {
            let fd_array = buffers[0].as_mut_ptr() as *mut i32;
            unsafe {
                *fd_array = read_fd as i32;
                *fd_array.add(1) = write_fd as i32;
            }
            0 // 成功
        } else {
            // 内存访问失败，清理已分配的文件描述符
            task_inner.close_fd(read_fd);
            task_inner.close_fd(write_fd);
            -14 // EFAULT
        }
    } else {
        -1
    }
}

/// lseek - 设置文件偏移量
pub fn sys_lseek(fd: usize, offset: isize, whence: usize) -> isize {
    const SEEK_SET: usize = 0;
    const SEEK_CUR: usize = 1;
    const SEEK_END: usize = 2;

    if let Some(task) = current_task() {
        let task_inner = task.inner_exclusive_access();
        if let Some(file_desc) = task_inner.get_fd(fd) {
            let current_offset = file_desc.offset.load(atomic::Ordering::Relaxed);
            let file_size = file_desc.inode.size();

            let new_offset = match whence {
                SEEK_SET => {
                    if offset < 0 {
                        return -22; // EINVAL
                    }
                    offset as u64
                }
                SEEK_CUR => {
                    let result = (current_offset as i64) + (offset as i64);
                    if result < 0 {
                        return -22; // EINVAL
                    }
                    result as u64
                }
                SEEK_END => {
                    let result = (file_size as i64) + (offset as i64);
                    if result < 0 {
                        return -22; // EINVAL
                    }
                    result as u64
                }
                _ => return -22, // EINVAL - Invalid whence
            };

            file_desc.offset.store(new_offset, atomic::Ordering::Release);
            new_offset as isize
        } else {
            -9 // EBADF - Bad file descriptor
        }
    } else {
        -1
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

    // Resolve the absolute path BEFORE getting exclusive access to avoid double borrow
    let absolute_path = get_vfs().resolve_relative_path(&path_str);

    // Check if the path exists and is a directory
    match get_vfs().open(&path_str) {
        Ok(inode) => {
            // Try to list the directory to verify it's actually a directory
            match inode.list_dir() {
                Ok(_) => {
                    // It's a directory, set the current working directory for the current task
                    if let Some(task) = current_task() {
                        let mut task_inner = task.inner_exclusive_access();
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

/// dup - 复制文件描述符
pub fn sys_dup(fd: usize) -> isize {
    if let Some(task) = current_task() {
        let mut task_inner = task.inner_exclusive_access();
        match task_inner.dup_fd(fd) {
            Some(new_fd) => new_fd as isize,
            None => -9, // EBADF - Bad file descriptor
        }
    } else {
        -1
    }
}

/// dup2 - 复制文件描述符到指定的文件描述符号
pub fn sys_dup2(oldfd: usize, newfd: usize) -> isize {
    if let Some(task) = current_task() {
        let mut task_inner = task.inner_exclusive_access();
        match task_inner.dup2_fd(oldfd, newfd) {
            Some(fd) => fd as isize,
            None => -9, // EBADF - Bad file descriptor
        }
    } else {
        -1
    }
}

/// flock - 对文件进行建议性锁定
pub fn sys_flock(fd: usize, operation: i32) -> isize {
    if let Some(task) = current_task() {
        let task_inner = task.inner_exclusive_access();
        if let Some(file_desc) = task_inner.get_fd(fd) {
            let inode = &file_desc.inode;
            let pid = task.get_pid();

            // Parse the operation
            let non_blocking = (operation & (LockOp::NonBlock as i32)) != 0;
            let lock_operation = operation & !4; // Remove LOCK_NB flag (4), keep other bits

            let lock_manager = get_file_lock_manager();

            match lock_operation {
                8 => { // LOCK_UN - Unlock
                    match lock_manager.unlock(inode, pid) {
                        Ok(()) => 0,
                        Err(LockError::NotLocked) => 0, // Not an error if already unlocked
                        Err(_) => -1,
                    }
                }
                1 => { // LOCK_SH - Shared lock
                    match lock_manager.try_lock(inode, LockType::Shared, pid, task.clone(), non_blocking) {
                        Ok(()) => 0,
                        Err(LockError::WouldBlock) => {
                            if non_blocking {
                                -11 // EAGAIN/EWOULDBLOCK
                            } else {
                                // In a real implementation, we would block the process here
                                // For now, we'll return EAGAIN to indicate blocking would occur
                                -11
                            }
                        }
                        Err(_) => -1,
                    }
                }
                2 => { // LOCK_EX - Exclusive lock
                    match lock_manager.try_lock(inode, LockType::Exclusive, pid, task.clone(), non_blocking) {
                        Ok(()) => 0,
                        Err(LockError::WouldBlock) => {
                            if non_blocking {
                                -11 // EAGAIN/EWOULDBLOCK
                            } else {
                                // In a real implementation, we would block the process here
                                // For now, we'll return EAGAIN to indicate blocking would occur
                                -11
                            }
                        }
                        Err(_) => -1,
                    }
                }
                _ => -22, // EINVAL - Invalid operation
            }
        } else {
            -9 // EBADF - Bad file descriptor
        }
    } else {
        -1
    }
}

/// 创建命名管道（FIFO）
pub fn sys_mkfifo(path: *const u8, mode: u32) -> isize {
    let token = current_user_token();
    let path_str = translated_c_string(token, path);
    let _ = mode; // Mode parameter is currently ignored

    match create_fifo(&path_str) {
        Ok(_) => 0,
        Err(e) => {
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

/// 修改文件权限
pub fn sys_chmod(path: *const u8, mode: u32) -> isize {
    let token = current_user_token();
    let path_str = translated_c_string(token, path);

    match get_vfs().open(&path_str) {
        Ok(inode) => {
            if let Some(task) = current_task() {
                let task_inner = task.inner_exclusive_access();
                
                // 检查权限：只有文件所有者或root用户可以修改权限
                let file_uid = inode.get_uid();
                if !task_inner.is_root() && task_inner.get_euid() != file_uid {
                    return -1; // EPERM
                }
                
                drop(task_inner); // 释放锁
                
                // 设置文件权限（只保留权限位，忽略文件类型位）
                let permission_bits = mode & 0o7777;
                match inode.set_mode(permission_bits) {
                    Ok(()) => 0,
                    Err(_) => -1,
                }
            } else {
                -1
            }
        }
        Err(_) => -2, // ENOENT
    }
}

/// 修改文件所有者
pub fn sys_chown(path: *const u8, uid: u32, gid: u32) -> isize {
    let token = current_user_token();
    let path_str = translated_c_string(token, path);

    match get_vfs().open(&path_str) {
        Ok(inode) => {
            if let Some(task) = current_task() {
                let task_inner = task.inner_exclusive_access();
                
                // 检查权限：只有文件所有者或root用户可以修改所有者
                let file_uid = inode.get_uid();
                if !task_inner.is_root() && task_inner.get_euid() != file_uid {
                    return -1; // EPERM
                }
                
                drop(task_inner); // 释放锁
                
                // 设置文件所有者
                let uid_result = if uid != u32::MAX {
                    inode.set_uid(uid)
                } else {
                    Ok(())
                };
                
                let gid_result = if gid != u32::MAX {
                    inode.set_gid(gid)
                } else {
                    Ok(())
                };
                
                match (uid_result, gid_result) {
                    (Ok(()), Ok(())) => 0,
                    _ => -1,
                }
            } else {
                -1
            }
        }
        Err(_) => -2, // ENOENT
    }
}
