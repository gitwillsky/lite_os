//! 文件系统操作模块 - 充分利用LiteOS的文件系统调用

use alloc::vec::Vec;
use alloc::string::String;
use alloc::string::ToString;
use user_lib::*;

/// 文件读取结果
pub struct FileReadResult {
    pub data: Vec<u8>,
    pub bytes_read: usize,
}

/// 文件信息结构
#[derive(Debug)]
pub struct FileInfo {
    pub size: u64,
    pub is_dir: bool,
    pub is_regular: bool,
}

/// 文件系统操作接口
pub struct FileSystem;

impl FileSystem {
    /// 读取整个文件内容
    /// 利用LiteOS的open/read/close系统调用
    pub fn read_file(path: &str) -> Result<Vec<u8>, String> {
        println!("Reading file: {}", path);
        
        // 打开文件
        let fd = open(path, 0); // O_RDONLY = 0
        if fd < 0 {
            return Err(alloc::format!("Failed to open file: {} (error: {})", path, fd));
        }
        
        // 获取文件大小
        let file_size = Self::get_file_size(fd as usize)?;
        
        // 分配缓冲区
        let mut buffer = Vec::with_capacity(file_size);
        unsafe { buffer.set_len(file_size); }
        
        // 读取文件内容
        let mut total_read = 0;
        while total_read < file_size {
            let bytes_to_read = core::cmp::min(4096, file_size - total_read);
            let bytes_read = read(fd as usize, &mut buffer[total_read..total_read + bytes_to_read]);
            
            if bytes_read < 0 {
                close(fd as usize);
                return Err(alloc::format!("Failed to read file: {} (error: {})", path, bytes_read));
            }
            
            if bytes_read == 0 {
                break; // EOF
            }
            
            total_read += bytes_read as usize;
        }
        
        // 关闭文件
        close(fd as usize);
        
        // 调整缓冲区大小到实际读取的字节数
        buffer.truncate(total_read);
        
        println!("Successfully read {} bytes from {}", total_read, path);
        Ok(buffer)
    }
    
    /// 写入文件内容
    /// 利用LiteOS的open/write/close系统调用
    pub fn write_file(path: &str, data: &[u8]) -> Result<(), String> {
        println!("Writing {} bytes to file: {}", data.len(), path);
        
        // 打开文件用于写入
        let fd = open(path, 1); // O_WRONLY = 1 (需要确认LiteOS的标志定义)
        if fd < 0 {
            return Err(alloc::format!("Failed to open file for writing: {} (error: {})", path, fd));
        }
        
        // 写入数据
        let mut total_written = 0;
        while total_written < data.len() {
            let bytes_to_write = core::cmp::min(4096, data.len() - total_written);
            let bytes_written = write(fd as usize, &data[total_written..total_written + bytes_to_write]);
            
            if bytes_written < 0 {
                close(fd as usize);
                return Err(alloc::format!("Failed to write file: {} (error: {})", path, bytes_written));
            }
            
            total_written += bytes_written as usize;
        }
        
        // 关闭文件
        close(fd as usize);
        
        println!("Successfully wrote {} bytes to {}", total_written, path);
        Ok(())
    }
    
    /// 获取文件大小
    fn get_file_size(fd: usize) -> Result<usize, String> {
        // 使用lseek获取文件大小
        let current_pos = lseek(fd, 0, 1); // SEEK_CUR = 1
        if current_pos < 0 {
            return Err("Failed to get current position".to_string());
        }
        
        let file_size = lseek(fd, 0, 2); // SEEK_END = 2
        if file_size < 0 {
            return Err("Failed to get file size".to_string());
        }
        
        // 恢复原始位置
        let restored_pos = lseek(fd, current_pos, 0); // SEEK_SET = 0
        if restored_pos < 0 {
            return Err("Failed to restore file position".to_string());
        }
        
        Ok(file_size as usize)
    }
    
    /// 获取文件信息
    /// 利用LiteOS的stat系统调用
    pub fn get_file_info(path: &str) -> Result<FileInfo, String> {
        println!("Getting file info: {}", path);
        
        let mut stat_buf = [0u8; 256]; // 假设stat结构大小
        let result = stat(path, &mut stat_buf);
        
        if result < 0 {
            return Err(alloc::format!("Failed to get file info: {} (error: {})", path, result));
        }
        
        // 解析stat结构 - 基于LiteOS的stat实现
        // 假设stat_buf的前几个字段是: mode(4字节), size(8字节)
        let mode = if stat_buf.len() >= 4 {
            u32::from_le_bytes([stat_buf[0], stat_buf[1], stat_buf[2], stat_buf[3]])
        } else {
            0
        };
        
        let size = if stat_buf.len() >= 12 {
            u64::from_le_bytes([
                stat_buf[4], stat_buf[5], stat_buf[6], stat_buf[7],
                stat_buf[8], stat_buf[9], stat_buf[10], stat_buf[11]
            ])
        } else {
            0
        };
        
        // 根据mode字段判断文件类型（简化的POSIX定义）
        let is_dir = mode & 0o040000 != 0;  // S_IFDIR
        let is_regular = mode & 0o100000 != 0;  // S_IFREG
        
        Ok(FileInfo {
            size,
            is_dir,
            is_regular: is_regular || (!is_dir && mode != 0), // 如果不是目录且mode不为0，视为普通文件
        })
    }
    
    /// 检查文件是否存在
    pub fn file_exists(path: &str) -> bool {
        Self::get_file_info(path).is_ok()
    }
    
    /// 创建目录
    /// 利用LiteOS的mkdir系统调用
    pub fn create_directory(path: &str) -> Result<(), String> {
        println!("Creating directory: {}", path);
        
        let result = mkdir(path);
        if result != 0 {
            Err(alloc::format!("Failed to create directory: {} (error: {})", path, result))
        } else {
            Ok(())
        }
    }
    
    /// 删除文件或目录
    /// 利用LiteOS的remove系统调用
    pub fn remove_path(path: &str) -> Result<(), String> {
        println!("Removing path: {}", path);
        
        let result = remove(path);
        if result != 0 {
            Err(alloc::format!("Failed to remove path: {} (error: {})", path, result))
        } else {
            Ok(())
        }
    }
    
    /// 列出目录内容
    /// 利用LiteOS的listdir系统调用
    pub fn list_directory(path: &str) -> Result<Vec<String>, String> {
        println!("Listing directory: {}", path);
        
        let mut buffer = [0u8; 4096];
        let result = listdir(path, &mut buffer);
        
        if result < 0 {
            return Err(alloc::format!("Failed to list directory: {} (error: {})", path, result));
        }
        
        // 解析目录列表 - 需要根据LiteOS的listdir返回格式来解析
        let mut entries = Vec::new();
        let mut current_pos = 0;
        
        while current_pos < result as usize {
            // 查找下一个null终止的字符串
            let mut end_pos = current_pos;
            while end_pos < buffer.len() && buffer[end_pos] != 0 {
                end_pos += 1;
            }
            
            if end_pos > current_pos {
                if let Ok(entry_name) = core::str::from_utf8(&buffer[current_pos..end_pos]) {
                    entries.push(entry_name.to_string());
                }
            }
            
            current_pos = end_pos + 1;
        }
        
        Ok(entries)
    }
    
    /// 改变当前工作目录
    /// 利用LiteOS的chdir系统调用
    pub fn change_directory(path: &str) -> Result<(), String> {
        println!("Changing directory to: {}", path);
        
        let result = chdir(path);
        if result != 0 {
            Err(alloc::format!("Failed to change directory: {} (error: {})", path, result))
        } else {
            Ok(())
        }
    }
    
    /// 获取当前工作目录
    /// 利用LiteOS的getcwd系统调用
    pub fn get_current_directory() -> Result<String, String> {
        let mut buffer = [0u8; 256];
        let result = getcwd(&mut buffer);
        
        if result < 0 {
            return Err(alloc::format!("Failed to get current directory (error: {})", result));
        }
        
        // 查找null终止符
        let end_pos = buffer.iter().position(|&x| x == 0).unwrap_or(buffer.len());
        
        match core::str::from_utf8(&buffer[..end_pos]) {
            Ok(path) => Ok(path.to_string()),
            Err(_) => Err("Invalid UTF-8 in directory path".to_string()),
        }
    }
    
    /// 创建符号链接或硬链接
    pub fn create_link(_target: &str, _link_path: &str) -> Result<(), String> {
        // LiteOS目前可能不支持链接，返回不支持错误
        Err("Link creation not supported in current LiteOS version".to_string())
    }
    
    /// 复制文件
    pub fn copy_file(src: &str, dst: &str) -> Result<(), String> {
        let data = Self::read_file(src)?;
        Self::write_file(dst, &data)?;
        Ok(())
    }
    
    /// 移动/重命名文件
    pub fn move_file(src: &str, dst: &str) -> Result<(), String> {
        // 先复制文件
        Self::copy_file(src, dst)?;
        // 然后删除原文件
        Self::remove_path(src)?;
        Ok(())
    }
}

/// 文件系统常量
pub mod fs_constants {
    pub const SEEK_SET: usize = 0;
    pub const SEEK_CUR: usize = 1;
    pub const SEEK_END: usize = 2;
    
    pub const O_RDONLY: u32 = 0;
    pub const O_WRONLY: u32 = 1;
    pub const O_RDWR: u32 = 2;
    pub const O_CREAT: u32 = 64;
    pub const O_TRUNC: u32 = 512;
    pub const O_APPEND: u32 = 1024;
}

/// 文件操作辅助函数
pub mod file_utils {
    
    /// 获取文件扩展名
    pub fn get_file_extension(path: &str) -> Option<&str> {
        if let Some(dot_pos) = path.rfind('.') {
            Some(&path[dot_pos + 1..])
        } else {
            None
        }
    }
    
    /// 获取文件名(不包含路径)
    pub fn get_filename(path: &str) -> &str {
        if let Some(slash_pos) = path.rfind('/') {
            &path[slash_pos + 1..]
        } else {
            path
        }
    }
    
    /// 获取目录路径(不包含文件名)
    pub fn get_directory(path: &str) -> &str {
        if let Some(slash_pos) = path.rfind('/') {
            &path[..slash_pos]
        } else {
            "."
        }
    }
    
    /// 检查是否为WASM文件
    pub fn is_wasm_file(path: &str) -> bool {
        get_file_extension(path).map_or(false, |ext| ext == "wasm")
    }
}