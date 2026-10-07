//! 每个挂载的属性（Linux `MNT_READONLY`/`MNT_NOSUID`/`MNT_NODEV`/`MNT_NOEXEC`）。

/// 一个挂载的属性位；位值与 Linux `statfs.f_flags`（`ST_*`）一致，因此可直接并入 statfs 与
/// `/proc/mounts` 的选项列表，不需要第二套编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MountFlags(u8);

impl MountFlags {
    pub(crate) const READ_ONLY: u8 = 1;
    pub(crate) const NOSUID: u8 = 2;
    pub(crate) const NODEV: u8 = 4;
    pub(crate) const NOEXEC: u8 = 8;
    const ALL: u8 = Self::READ_ONLY | Self::NOSUID | Self::NODEV | Self::NOEXEC;

    /// 从 `ST_*` 位构造；高位被丢弃。
    pub(crate) const fn from_bits(bits: u64) -> Self {
        Self((bits & Self::ALL as u64) as u8)
    }

    pub(crate) const fn bits(self) -> u64 {
        self.0 as u64
    }

    pub(crate) const fn read_only(self) -> bool {
        self.0 & Self::READ_ONLY != 0
    }

    pub(crate) const fn nosuid(self) -> bool {
        self.0 & Self::NOSUID != 0
    }

    pub(crate) const fn nodev(self) -> bool {
        self.0 & Self::NODEV != 0
    }

    pub(crate) const fn noexec(self) -> bool {
        self.0 & Self::NOEXEC != 0
    }
}
