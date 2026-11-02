//! Linux v7.1 clone/clone3 UAPI；只拥有常量与 layout，资源、权限和 publication 属于 kernel task。

pub const NEWTIME: u64 = 0x80;
pub const VM: u64 = 0x100;
pub const FS: u64 = 0x200;
pub const FILES: u64 = 0x400;
pub const SIGHAND: u64 = 0x800;
pub const PIDFD: u64 = 0x1000;
pub const VFORK: u64 = 0x4000;
pub const PARENT: u64 = 0x8000;
pub const THREAD: u64 = 0x10000;
pub const NEWNS: u64 = 0x20000;
pub const SYSVSEM: u64 = 0x40000;
pub const SETTLS: u64 = 0x80000;
pub const PARENT_SETTID: u64 = 0x100000;
pub const CHILD_CLEARTID: u64 = 0x200000;
pub const DETACHED: u64 = 0x400000;
pub const CHILD_SETTID: u64 = 0x1000000;
pub const NEWUTS: u64 = 0x4000000;
pub const NEWIPC: u64 = 0x8000000;
pub const NEWUSER: u64 = 0x10000000;
pub const NEWPID: u64 = 0x20000000;
pub const NEWNET: u64 = 0x40000000;
pub const CLEAR_SIGHAND: u64 = 0x100000000;
pub const INTO_CGROUP: u64 = 0x200000000;
pub const SIZE_VER0: usize = 64;
pub const SIZE_VER1: usize = 80;
pub const SIZE_VER2: usize = 88;
pub const MAX_SET_TID: usize = 32;

/// clone3 固定的 11 个 aligned-u64 字段；短版本字段按 Linux copy_struct_from_user 零扩展。
#[derive(Debug, Clone, Copy, Default)]
pub struct CloneArgs {
    pub flags: u64,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub exit_signal: u64,
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
    pub set_tid: u64,
    pub set_tid_size: u64,
    pub cgroup: u64,
}

impl CloneArgs {
    /// 解码已完成 user-copy 的当前或短版前缀；未知尾部必须由 syscall 验证为零。
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if !(SIZE_VER0..=SIZE_VER2).contains(&bytes.len()) {
            return None;
        }
        let mut storage = [0; SIZE_VER2];
        storage[..bytes.len()].copy_from_slice(bytes);
        let mut values = storage
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_ne_bytes(*word));
        Some(Self {
            flags: values.next()?,
            pidfd: values.next()?,
            child_tid: values.next()?,
            parent_tid: values.next()?,
            exit_signal: values.next()?,
            stack: values.next()?,
            stack_size: values.next()?,
            tls: values.next()?,
            set_tid: values.next()?,
            set_tid_size: values.next()?,
            cgroup: values.next()?,
        })
    }
}
