//! 备份文件的唯一命名、确认与保留逻辑；保存内容就是 Configuration JSON，不另建索引。

use alloc::{format, vec::Vec};
use filesystem_protocol::StorageService;
use native_runtime::native_abi::{self, Status};
use router_protocol::backup::{Entry, Id};

fn name(id: Id, automatic: bool) -> Vec<u8> {
    format!(
        "backup-{}-{id}.json",
        if automatic { "auto" } else { "manual" }
    )
    .into_bytes()
}

/// 枚举仅属于本子系统的规范文件；临时文件、用户设置和未知名字不参与清理。
pub(super) fn list(store: &impl StorageService) -> Result<Vec<Entry>, Status> {
    let files = store.list()?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(files.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for file in files {
        let Some(text) = core::str::from_utf8(&file.name).ok() else {
            continue;
        };
        let (automatic, id) = if let Some(id) = text.strip_prefix("backup-auto-") {
            (true, id)
        } else if let Some(id) = text.strip_prefix("backup-manual-") {
            (false, id)
        } else {
            continue;
        };
        let Some(id) = id
            .strip_suffix(".json")
            .and_then(|id| id.parse::<Id>().ok())
        else {
            continue;
        };
        entries.push(Entry {
            id,
            automatic,
            size: file.size,
            modified_seconds: file.modified_seconds,
        });
    }
    entries.sort_unstable_by_key(|entry| core::cmp::Reverse(entry.id));
    Ok(entries)
}

fn find(store: &impl StorageService, id: Id) -> Result<Entry, Status> {
    let entries = list(store)?;
    let mut found = entries.into_iter().filter(|entry| entry.id == id);
    let entry = found.next().ok_or(native_abi::ERR_NOT_FOUND)?;
    if found.next().is_some() {
        return Err(native_abi::ERR_BAD_STATE);
    }
    Ok(entry)
}

pub(super) fn read(store: &impl StorageService, id: Id) -> Result<Vec<u8>, Status> {
    let entry = find(store, id)?;
    store
        .read(&name(id, entry.automatic))?
        .ok_or(native_abi::ERR_NOT_FOUND)
}

pub(super) fn remove(store: &impl StorageService, id: Id) -> Result<(), Status> {
    let entry = find(store, id)?;
    store.remove(&name(id, entry.automatic))
}

/// 错误可能已发布，因此先读回/同步确认，重试同一身份，不覆盖既有不同内容。
/// touched 表示可能改变了备份目录，缺少它会在回复丢失后漏发刷新通知。
pub(super) fn save(
    store: &impl StorageService,
    id: Id,
    automatic: bool,
    bytes: &[u8],
    touched: &mut bool,
) -> Result<(), Status> {
    let name = name(id, automatic);
    match store.read(&name)? {
        Some(current) if current == bytes => return store.sync(),
        Some(_) => return Err(native_abi::ERR_ALREADY_BOUND),
        None => {}
    }
    *touched = true;
    match store.create(&name, bytes) {
        Ok(()) => Ok(()),
        Err(cause) => match store.read(&name)? {
            Some(current) if current == bytes => store.sync(),
            Some(_) => Err(native_abi::ERR_ALREADY_BOUND),
            None => Err(cause),
        },
    }
}

/// 仅在新副本已确认后调用；保留最新自动副本，手动副本永远不受自动策略删除。
pub(super) fn prune(
    store: &impl StorageService,
    retain: u32,
    touched: &mut bool,
) -> Result<(), Status> {
    for entry in list(store)?
        .into_iter()
        .filter(|entry| entry.automatic)
        .skip(retain as usize)
    {
        *touched = true;
        store.remove(&name(entry.id, true))?;
    }
    // 上次 UNLINK 可能已生效但目录同步失败；重试枚举已看不到它，仍须确认命名持久化。
    store.sync()
}
