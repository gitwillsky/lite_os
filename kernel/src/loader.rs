use alloc::vec::Vec;

use crate::fs::vfs::get_vfs;


/// 从文件系统加载程序二进制文件
pub fn load_program_from_fs(path: &str) -> Option<Vec<u8>> {
    match get_vfs().open(path) {
        Ok(inode) => {
            let size = inode.size() as usize;
            if size == 0 {
                return None;
            }
            let mut buffer = alloc::vec![0u8; size];
            match inode.read_at(0, &mut buffer) {
                Ok(_) => Some(buffer),
                Err(_) => None,
            }
        }
        Err(_) => None,
    }
}

/// 标准ELF加载接口 - 从文件系统加载程序
pub fn get_app_data_by_name(app_name: &str) -> Option<Vec<u8>> {
    // 构造程序文件路径 - 支持多种命名格式
    let paths = [
        alloc::format!("/{}.bin", app_name.to_uppercase()),
        alloc::format!("/{}.bin", app_name.to_lowercase()),
        alloc::format!("/{}", app_name),
    ];

    for path in &paths {
        if let Some(data) = load_program_from_fs(path) {
            return Some(data);
        }
    }

    None
}
