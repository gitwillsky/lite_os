use alloc::vec::Vec;

use crate::fs::vfs::vfs;

/// 从文件系统加载程序二进制文件
pub fn load_program_from_fs(path: &str) -> Option<Vec<u8>> {
    debug!("Attempting to load program from: {}", path);
    match vfs().open(path) {
        Ok(inode) => {
            let size = inode.size() as usize;
            debug!("File size: {}", size);
            if size == 0 {
                debug!("File size is 0, returning None");
                return None;
            }
            let mut buffer = alloc::vec![0u8; size];
            match inode.read_at(0, &mut buffer) {
                Ok(bytes_read) => {
                    debug!("Successfully read {} bytes", bytes_read);
                    Some(buffer)
                }
                Err(e) => {
                    debug!("Failed to read file: {:?}", e);
                    None
                }
            }
        }
        Err(e) => {
            debug!("Failed to open file: {:?}", e);
            None
        }
    }
}

/// 标准ELF加载接口 - 从文件系统加载程序
pub fn get_app_data_by_name(app_name: &str) -> Option<Vec<u8>> {
    debug!("Looking for app: {}", app_name);

    if let Some(data) = load_program_from_fs(app_name) {
        debug!("Successfully loaded from path: {}", app_name);
        return Some(data);
    }

    debug!("Failed to load app: {}", app_name);
    None
}
