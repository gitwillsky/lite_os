use alloc::sync::Arc;
use alloc::vec::Vec;

use super::mount_view::MountView;
use super::{FileSystemError, FileSystemStatistics, MountFlags};

/// 同一实际 namespace 拓扑的 Linux proc 展示格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountTableKind {
    Mounts,
    Info,
    Stats,
}

fn write_mounts_record(
    output: &mut Vec<u8>,
    source: &[u8],
    target: &[u8],
    statistics: &FileSystemStatistics,
    flags: MountFlags,
    options: &[u8],
) -> Result<(), FileSystemError> {
    let escaped_fields = source
        .len()
        .checked_add(target.len())
        .and_then(|length| length.checked_add(statistics.type_name.len()))
        .and_then(|length| length.checked_mul(4))
        .ok_or(FileSystemError::OutOfMemory)?;
    let required = escaped_fields
        .checked_add(options.len())
        .and_then(|length| length.checked_add(80))
        .ok_or(FileSystemError::OutOfMemory)?;
    output
        .try_reserve(required)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    write_field(output, source);
    output.push(b' ');
    write_field(output, target);
    output.push(b' ');
    write_field(output, statistics.type_name.as_bytes());
    // 文件系统自身只读（sysfs/devtmpfs）与挂载只读都显示为 `ro`。
    let read_only = statistics.flags & 1 != 0 || flags.read_only();
    output.extend_from_slice(if read_only { b" ro" } else { b" rw" });
    for (set, name) in [
        (flags.nosuid(), &b",nosuid"[..]),
        (flags.nodev(), b",nodev"),
        (flags.noexec(), b",noexec"),
        (flags.noatime(), b",noatime"),
        (flags.nodiratime(), b",nodiratime"),
        (flags.relatime(), b",relatime"),
    ] {
        if set {
            output.extend_from_slice(name);
        }
    }
    output.extend_from_slice(options);
    output.extend_from_slice(b" 0 0\n");
    Ok(())
}

fn write_field(output: &mut Vec<u8>, field: &[u8]) {
    for byte in field {
        match byte {
            b' ' => output.extend_from_slice(b"\\040"),
            b'\t' => output.extend_from_slice(b"\\011"),
            b'\n' => output.extend_from_slice(b"\\012"),
            b'\\' => output.extend_from_slice(b"\\134"),
            b'#' => output.extend_from_slice(b"\\043"),
            byte => output.push(*byte),
        }
    }
}

/// 单一 codec 渲染真实 view；每次先预留完整最坏长度，发表一行期间不再分配。
pub(super) fn write_mount_record(
    output: &mut Vec<u8>,
    view: &MountView,
    viewer_root: &Arc<super::OpenedFile>,
    target: &[u8],
    filesystem_root: &[u8],
    statistics: &FileSystemStatistics,
    kind: MountTableKind,
) -> Result<(), FileSystemError> {
    let options = if kind == MountTableKind::Stats {
        Vec::new()
    } else {
        view.filesystem.filesystem.show_mount_options()?
    };
    if kind == MountTableKind::Mounts {
        return write_mounts_record(
            output,
            &view.source,
            target,
            statistics,
            view.flags(),
            &options,
        );
    }
    // 行发表前完成所有可能失败的inode/传播投影；emit期间只有已预留Vec和scalar codec。
    let info = if kind == MountTableKind::Info {
        let device = super::super::DeviceNumber::decode(view.root.inode().metadata()?.filesystem);
        let shared = super::mount_propagation::group(view).map(|group| group.id);
        let master = super::mount_propagation::master(view);
        let master_id = master
            .as_ref()
            .map(|master| super::mount_propagation::group(master).map_or(0, |group| group.id));
        let dominating = if master.is_some() {
            super::mount_propagation::dominating_id(view, viewer_root)?
        } else {
            0
        };
        Some((
            device,
            shared,
            master_id,
            dominating,
            super::mount_propagation::unbindable(view),
        ))
    } else {
        None
    };
    let required = view
        .source
        .len()
        .checked_add(target.len())
        .and_then(|n| n.checked_add(filesystem_root.len()))
        .and_then(|n| n.checked_add(statistics.type_name.len()))
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| n.checked_add(options.len()))
        // 两个mount ID、dev_t、全部view flags和四种传播tag最多占固定256bytes。
        .and_then(|n| n.checked_add(256))
        .ok_or(FileSystemError::OutOfMemory)?;
    output
        .try_reserve(required)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    if kind == MountTableKind::Stats {
        output.extend_from_slice(b"device ");
        write_field(output, &view.source);
        output.extend_from_slice(b" mounted on ");
        write_field(output, target);
        output.extend_from_slice(b" with fstype ");
        write_field(output, statistics.type_name.as_bytes());
        output.push(b'\n');
        return Ok(());
    }
    let (device, shared, master, dominating, unbindable) =
        info.expect("mountinfo projection prepared");
    decimal(output, view.id);
    output.push(b' ');
    decimal(
        output,
        view.attachment()
            .and_then(|attachment| attachment.parent.upgrade())
            .map_or(view.id, |parent| parent.id),
    );
    output.push(b' ');
    // st_dev 与 inode owner 一致；physical claim 是资源准入事实，不替代 inode 的 dev_t。
    decimal(output, u64::from(device.major));
    output.push(b':');
    decimal(output, u64::from(device.minor));
    output.push(b' ');
    write_field(output, filesystem_root);
    output.push(b' ');
    write_field(output, target);
    output.push(b' ');
    // mountinfo分开报告per-view与superblock RO；root-umount改SB不能把view的rw重写成ro。
    let flags = view.state.lock().flags;
    write_flags(output, flags);
    // propagation 字段由实际 view 的拓扑状态产生，不从 MS_* 请求字符串缓存。
    if let Some(group) = shared {
        output.extend_from_slice(b" shared:");
        decimal(output, group);
    }
    if let Some(id) = master {
        output.extend_from_slice(b" master:");
        decimal(output, id);
        if dominating != 0 && dominating != id {
            output.extend_from_slice(b" propagate_from:");
            decimal(output, dominating);
        }
    }
    if unbindable {
        output.extend_from_slice(b" unbindable");
    }
    output.extend_from_slice(b" - ");
    write_field(output, statistics.type_name.as_bytes());
    output.push(b' ');
    write_field(output, &view.source);
    output.extend_from_slice(
        if view.filesystem.state.lock().read_only || statistics.flags & 1 != 0 {
            b" ro"
        } else {
            b" rw"
        },
    );
    output.extend_from_slice(&options);
    output.push(b'\n');
    Ok(())
}
fn decimal(output: &mut Vec<u8>, mut value: u64) {
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    output.extend_from_slice(&digits[start..]);
}
fn write_flags(output: &mut Vec<u8>, flags: MountFlags) {
    output.extend_from_slice(if flags.read_only() { b"ro" } else { b"rw" });
    for (set, name) in [
        (flags.nosuid(), &b",nosuid"[..]),
        (flags.nodev(), &b",nodev"[..]),
        (flags.noexec(), &b",noexec"[..]),
        (flags.noatime(), &b",noatime"[..]),
        (flags.nodiratime(), &b",nodiratime"[..]),
        (flags.relatime(), &b",relatime"[..]),
    ] {
        if set {
            output.extend_from_slice(name);
        }
    }
}
