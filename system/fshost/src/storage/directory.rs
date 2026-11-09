//! 私有目录枚举：只返回合法直接普通文件，每次 LOOKUP 与 OPENDIR 都在错误路径归还。

use super::*;
use filesystem_protocol::{StorageEntry, directory::Record, storage_name};

const OPENDIR: u32 = 27;
const READDIR: u32 = 28;
const RELEASEDIR: u32 = 29;

impl DataVolume {
    fn describe_file(&self, directory: u64, name: &[u8]) -> Result<Option<StorageEntry>, Status> {
        let entry = match self.call(LOOKUP, directory, &Body::new().name(name).0) {
            Ok(entry) => entry,
            Err(Failure::Errno(ENOENT)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let node = u64_at(&entry, 0)?;
        let result = (|| {
            if entry.len() < ENTRY_OUT {
                return Err(native_abi::ERR_IO);
            }
            let mode = u32::from_ne_bytes(entry[100..104].try_into().expect("fuse attr"));
            if mode & 0o170000 != 0o100000 {
                return Ok(None);
            }
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(name.len())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            owned.extend_from_slice(name);
            Ok(Some(StorageEntry {
                name: owned,
                size: u64_at(&entry, 48)?,
                modified_seconds: u64_at(&entry, 72)? as i64,
            }))
        })();
        self.forget(node);
        result
    }

    pub(super) fn list_files(&self, directory: u64) -> Result<Vec<StorageEntry>, Status> {
        let opened = self
            .call(OPENDIR, directory, &Body::new().u32(0).u32(0).0)
            .map_err(Status::from)?;
        let handle = u64_at(&opened, 0)?;
        let result = (|| {
            let mut files = Vec::new();
            let mut offset = 0;
            loop {
                let request = Body::new()
                    .u64(handle)
                    .u64(offset)
                    .u32(CHUNK as u32)
                    .u32(0)
                    .u64(0)
                    .u32(0)
                    .u32(0);
                let bytes = self
                    .call(READDIR, directory, &request.0)
                    .map_err(Status::from)?;
                if bytes.is_empty() {
                    return Ok(files);
                }
                if bytes.len() > CHUNK {
                    return Err(native_abi::ERR_IO);
                }
                let start = offset;
                let mut remaining = bytes.as_slice();
                while !remaining.is_empty() {
                    let record = Record::decode(remaining)?;
                    remaining = &remaining[record.bytes..];
                    offset = record.offset;
                    if storage_name(record.name).is_err() {
                        continue;
                    }
                    if let Some(file) = self.describe_file(directory, record.name)? {
                        files
                            .try_reserve(1)
                            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                        files.push(file);
                    }
                }
                if offset == start {
                    return Err(native_abi::ERR_IO);
                }
            }
        })();
        // 服务目录本身的 lookup 由存储能力保活；关闭句柄不能 FORGET 那个长期引用。
        let released = self
            .call(
                RELEASEDIR,
                directory,
                &Body::new().u64(handle).u32(0).u32(0).u64(0).0,
            )
            .map_err(Status::from);
        match (result, released) {
            (Ok(files), Ok(_)) => Ok(files),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}
