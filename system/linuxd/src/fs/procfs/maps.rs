//! maps 的文本编解码；MM 生命周期与授权由 task owner 处理，路径事实由实际文件提供。
use super::{FileSystemError, OpenedFile, ProcMapFormat, ProcText};
use alloc::{sync::Arc, vec::Vec};
use core::fmt::Write;

/// 一次 MM 快照中的映射；不保活 Thread、MM 或另一份 VMA 索引。
pub(crate) struct ProcMapping {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) permissions: [bool; 3],
    pub(crate) shared: bool,
    pub(crate) offset: u64,
    pub(crate) file: Option<Arc<OpenedFile>>,
    pub(crate) name: Option<Arc<[u8]>>,
    pub(crate) label: Option<&'static [u8]>,
    pub(crate) statistics: crate::memory::MappingStatistics,
}

/// 成功 open 时绑定的 MM 观察引用；exec 后不切换到新 MM，最后实际 user 退出后返回空列表。
pub(crate) trait ProcMemoryMaps: Send + Sync {
    /// 取得映射快照；分配失败返回 OutOfMemory，不触发内容页 fault-in。
    fn snapshot(&self, detailed: bool) -> Result<Vec<ProcMapping>, FileSystemError>;
}

pub(super) fn contents(
    mappings: &[ProcMapping],
    root: &Arc<OpenedFile>,
    format: ProcMapFormat,
) -> Result<Vec<u8>, FileSystemError> {
    let mut output = ProcText::new();
    if format == ProcMapFormat::Rollup {
        if let (Some(first), Some(last)) = (mappings.first(), mappings.last()) {
            let mut statistics = crate::memory::MappingStatistics::default();
            for mapping in mappings {
                statistics.add(&mapping.statistics);
            }
            let rollup = ProcMapping {
                start: first.start,
                end: last.end,
                permissions: [false; 3],
                shared: false,
                offset: 0,
                file: None,
                name: None,
                label: Some(b"[rollup]"),
                statistics,
            };
            header(&mut output, &rollup, root)?;
            memory(&mut output, &statistics, true)?;
        }
        return Ok(output.finish());
    }
    for area in mappings {
        header(&mut output, area, root)?;
        if format == ProcMapFormat::Smaps {
            for (name, bytes) in [
                ("Size", (area.end - area.start) as u64),
                ("KernelPageSize", crate::memory::PAGE_SIZE as u64),
                ("MMUPageSize", crate::memory::PAGE_SIZE as u64),
            ] {
                value(&mut output, name, bytes)?;
            }
            memory(&mut output, &area.statistics, false)?;
        }
    }
    Ok(output.finish())
}

fn value(output: &mut ProcText, name: &str, bytes: u64) -> Result<(), FileSystemError> {
    writeln!(output, "{name}: {:>8} kB", bytes / 1024).map_err(|_| FileSystemError::OutOfMemory)
}

fn memory(
    output: &mut ProcText,
    statistics: &crate::memory::MappingStatistics,
    rollup: bool,
) -> Result<(), FileSystemError> {
    for (name, bytes) in [
        ("Rss", statistics.rss),
        ("Pss", statistics.pss >> 12),
        ("Pss_Dirty", statistics.pss_dirty >> 12),
        ("Shared_Clean", statistics.shared_clean),
        ("Shared_Dirty", statistics.shared_dirty),
        ("Private_Clean", statistics.private_clean),
        ("Private_Dirty", statistics.private_dirty),
        ("Anonymous", statistics.anonymous),
    ] {
        value(output, name, bytes)?;
    }
    if rollup {
        for (name, bytes) in [
            ("Pss_Anon", statistics.pss_anon >> 12),
            ("Pss_File", statistics.pss_file >> 12),
            ("Pss_Shmem", statistics.pss_shmem >> 12),
        ] {
            value(output, name, bytes)?;
        }
    }
    Ok(())
}

fn header(
    output: &mut ProcText,
    area: &ProcMapping,
    root: &Arc<OpenedFile>,
) -> Result<(), FileSystemError> {
    let start = output.len();
    let (device, inode) = area
        .file
        .as_ref()
        .map_or(Ok((0, 0)), |file| file.inode().identity())?;
    let device = crate::fs::DeviceNumber::decode(device);
    write!(
        output,
        "{:08x}-{:08x} {}{}{}{} {:08x} {:02x}:{:02x} {} ",
        area.start,
        area.end,
        if area.permissions[0] { 'r' } else { '-' },
        if area.permissions[1] { 'w' } else { '-' },
        if area.permissions[2] { 'x' } else { '-' },
        if area.shared { 's' } else { 'p' },
        area.offset,
        device.major,
        device.minor,
        inode
    )
    .map_err(|_| FileSystemError::OutOfMemory)?;
    let prefix_len = output.len() - start;
    let has_name = area.file.is_some() || area.name.is_some() || area.label.is_some();
    if has_name {
        // Linux seq_setwidth：路径至少空一格；64 位目标的标准路径起始列是 73。
        let spaces = 72usize.saturating_sub(prefix_len);
        output.write_bytes(&[b' '; 72][..spaces])?;
        if let Some(name) = &area.name {
            output.write_bytes(if area.shared {
                b"[anon_shmem:"
            } else {
                b"[anon:"
            })?;
            output.write_bytes(name)?;
            output.write_bytes(b"]")?;
        } else if let Some(file) = &area.file {
            let path = super::vfs().opened_path(file, root)?;
            for part in path.split_inclusive(|byte| *byte == b'\n') {
                if let Some(before) = part.strip_suffix(b"\n") {
                    output.write_bytes(before)?;
                    output.write_bytes(b"\\012")?;
                } else {
                    output.write_bytes(part)?;
                }
            }
        } else if let Some(label) = area.label {
            output.write_bytes(label)?;
        }
    }
    output.write_bytes(b"\n")?;
    Ok(())
}
