use alloc::vec::Vec;

use crate::fs::vfs::vfs;


/// 从文件系统加载程序二进制文件
pub fn load_program_from_fs(path: &str) -> Option<Vec<u8>> {
    debug!("[LOADER] Attempting to load program from: {}", path);
    match vfs().open(path) {
        Ok(inode) => {
            let size = inode.size() as usize;
            debug!("[LOADER] File size: {}", size);
            if size == 0 {
                debug!("[LOADER] File size is 0, returning None");
                return None;
            }
            let mut buffer = alloc::vec![0u8; size];
            match inode.read_at(0, &mut buffer) {
                Ok(bytes_read) => {
                    debug!("[LOADER] Successfully read {} bytes", bytes_read);
                    Some(buffer)
                },
                Err(e) => {
                    debug!("[LOADER] Failed to read file: {:?}", e);
                    None
                }
            }
        }
        Err(e) => {
            debug!("[LOADER] Failed to open file: {:?}", e);
            None
        }
    }
}

/// 标准ELF加载接口 - 从文件系统加载程序
pub fn get_app_data_by_name(app_name: &str) -> Option<Vec<u8>> {
    debug!("[LOADER] Looking for app: {}", app_name);
    // 构造程序文件路径 - 优先尝试ELF文件，再尝试.bin文件
    let paths = [
        alloc::format!("/{}", app_name),                 // ELF文件：/initproc
        alloc::format!("/{}", app_name.to_uppercase()),  // ELF文件：/INITPROC
        alloc::format!("/{}", app_name.to_lowercase()),  // ELF文件：/initproc
    ];

    for path in &paths {
        debug!("[LOADER] Trying path: {}", path);
        if let Some(data) = load_program_from_fs(path) {
            debug!("[LOADER] Successfully loaded from path: {}", path);
            return Some(data);
        }
    }

    debug!("[LOADER] Failed to load app: {}", app_name);
    None
}
