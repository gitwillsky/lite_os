//! Linux native64 sysvipc seq reader；actual open 捕获 IPC/PID/UserNS，连续读取不重新渲染已消费行。
use crate::{
    fs::chardev::{DeviceError, UserOutput},
    ipc::{IpcDescription, IpcNamespace, IpcTable},
    namespace::{PidIdentity, PidNamespace, UserNamespace},
};
use alloc::sync::Arc;
use core::fmt::{self, Write};
use syscall_abi::errno;

// 标准 native64 shm 是最长行：16 个固定宽度 scalar，总长小于512；不随对象数量增长。
const RECORD_BYTES: usize = 512;
#[derive(Clone, Copy)]
enum NextRecord {
    Header,
    Slot(u32),
}
/// 每个实际 open 的 seq cursor/未交付记录；OFD position 仍由 file owner 独占。
/// 这里的 position 仅校验连续 read；seek/pread offset 不连续时从 header 重新 traverse，复用同一 codec。
pub(super) struct IpcReadCursor {
    next: NextRecord,
    bytes: [u8; RECORD_BYTES],
    length: usize,
    consumed: usize,
    position: u64,
}
impl IpcReadCursor {
    pub(super) const fn new() -> Self {
        Self {
            next: NextRecord::Header,
            bytes: [0; RECORD_BYTES],
            length: 0,
            consumed: 0,
            position: 0,
        }
    }
    fn fill(
        &mut self,
        namespace: &IpcNamespace,
        table: IpcTable,
        user: &UserNamespace,
        pid: &PidNamespace,
    ) -> Result<bool, DeviceError> {
        if self.consumed < self.length {
            return Ok(true);
        }
        self.length = 0;
        self.consumed = 0;
        if matches!(self.next, NextRecord::Header) {
            let header = header(table).as_bytes();
            self.bytes[..header.len()].copy_from_slice(header);
            self.length = header.len();
            self.next = NextRecord::Slot(0);
            return Ok(true);
        }
        let NextRecord::Slot(cursor) = self.next else {
            unreachable!("header already encoded");
        };
        let Some((next, row)) = namespace.next_description(table, cursor) else {
            return Ok(false);
        };
        let mut output = RecordWriter {
            bytes: &mut self.bytes,
            length: &mut self.length,
        };
        if let Err(error) = format_row(&mut output, row, user, pid) {
            // codec 失败没有发表 partial record；下次仍从同一 actual slot 重试。
            self.length = 0;
            return Err(error);
        }
        self.next = NextRecord::Slot(next);
        Ok(true)
    }
    fn consume(&mut self, count: usize) {
        assert!(
            count <= self.length - self.consumed,
            "sysvipc copy exceeds selected record"
        );
        self.consumed += count;
        self.position = self
            .position
            .checked_add(count as u64)
            .expect("sysvipc position overflow");
    }
    /// 顺序 read 只继续当前记录/下一 actual slot；不连续 offset 才重置并按 native seq traversal 跳过 prefix。
    /// user copy fault 保留真实 partial progress，下一次 OFD offset 对应同一未消费 suffix。
    pub(super) fn read(
        &mut self,
        output: &mut dyn UserOutput,
        offset: u64,
        namespace: &IpcNamespace,
        table: IpcTable,
        user: &UserNamespace,
        pid: &PidNamespace,
    ) -> Result<(), DeviceError> {
        if output.remaining() == 0 {
            return Ok(());
        }
        if offset != self.position {
            *self = Self::new();
            while self.position < offset {
                if !self.fill(namespace, table, user, pid)? {
                    return Ok(());
                }
                let skip =
                    (offset - self.position).min((self.length - self.consumed) as u64) as usize;
                self.consume(skip);
            }
        }
        while output.remaining() != 0 && self.fill(namespace, table, user, pid)? {
            let count = (self.length - self.consumed).min(output.remaining());
            let before = output.remaining();
            let result = output.write(&self.bytes[self.consumed..self.consumed + count]);
            let copied = before
                .checked_sub(output.remaining())
                .expect("user output cursor moved backwards");
            self.consume(copied);
            result.map_err(|_| DeviceError::Errno(errno::EFAULT))?;
            assert_eq!(
                copied, count,
                "successful sysvipc copy did not advance its actual cursor"
            );
        }
        Ok(())
    }
}
struct RecordWriter<'a> {
    bytes: &'a mut [u8; RECORD_BYTES],
    length: &'a mut usize,
}
impl Write for RecordWriter<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.length.checked_add(text.len()).ok_or(fmt::Error)?;
        let target = self.bytes.get_mut(*self.length..end).ok_or(fmt::Error)?;
        target.copy_from_slice(text.as_bytes());
        *self.length = end;
        Ok(())
    }
}
fn header(table: IpcTable) -> &'static str {
    match table {
        IpcTable::Message => {
            "       key      msqid perms      cbytes       qnum lspid lrpid   uid   gid  cuid  cgid      stime      rtime      ctime\n"
        }
        IpcTable::Semaphore => {
            "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n"
        }
        IpcTable::SharedMemory => {
            "       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime                   rss                  swap\n"
        }
    }
}
fn format_row(
    output: &mut RecordWriter<'_>,
    row: IpcDescription,
    user: &UserNamespace,
    pid: &PidNamespace,
) -> Result<(), DeviceError> {
    let uid = |value| user.map_kernel_id(true, value).unwrap_or(65534);
    let gid = |value| user.map_kernel_id(false, value).unwrap_or(65534);
    let number =
        |value: &Option<Arc<PidIdentity>>| value.as_ref().map_or(0, |value| value.number_in(pid));
    let formatted = match row {
        IpcDescription::Message(row) => {
            let perm = row.permission;
            writeln!(
                output,
                "{:10} {:10}  {:4o}  {:10} {:10} {:5} {:5} {:5} {:5} {:5} {:5} {:10} {:10} {:10}",
                perm.key,
                row.id,
                perm.mode,
                row.bytes,
                row.messages,
                number(&row.last_sender),
                number(&row.last_receiver),
                uid(perm.uid),
                gid(perm.gid),
                uid(perm.creator_uid),
                gid(perm.creator_gid),
                row.send_time,
                row.receive_time,
                row.change_time
            )
        }
        IpcDescription::Semaphore(row) => {
            let perm = row.permission;
            writeln!(
                output,
                "{:10} {:10}  {:4o} {:10} {:5} {:5} {:5} {:5} {:10} {:10}",
                perm.key,
                row.id,
                perm.mode,
                row.count,
                uid(perm.uid),
                gid(perm.gid),
                uid(perm.creator_uid),
                gid(perm.creator_gid),
                row.operation_time,
                row.change_time
            )
        }
        IpcDescription::SharedMemory {
            snapshot: row,
            resident_pages,
            swapped_pages,
        } => {
            let perm = row.permission;
            writeln!(
                output,
                "{:10} {:10}  {:4o} {:21} {:5} {:5}  {:5} {:5} {:5} {:5} {:5} {:10} {:10} {:10} {:21} {:21}",
                perm.key,
                row.id,
                perm.mode,
                row.size,
                row.creator.number_in(pid),
                number(&row.last_operator),
                row.attaches,
                uid(perm.uid),
                gid(perm.gid),
                uid(perm.creator_uid),
                gid(perm.creator_gid),
                row.attach_time,
                row.detach_time,
                row.change_time,
                resident_pages as u64 * crate::memory::PAGE_SIZE as u64,
                swapped_pages as u64 * crate::memory::PAGE_SIZE as u64
            )
        }
    };
    formatted.map_err(|_| DeviceError::Errno(errno::EOVERFLOW))
}
