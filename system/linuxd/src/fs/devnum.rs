//! Linux dev_t 的唯一设备编号 codec；driver registry 与文件 metadata 使用同一值。
/// Linux character-device 号（`dev_t` 的 major/minor）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DeviceNumber {
    pub(crate) major: u32,
    pub(crate) minor: u32,
}

impl DeviceNumber {
    pub(crate) const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// 解码 Linux `new_decode_dev` 的 64-bit `dev_t`（[`Self::encode`] 的逆）。
    pub(crate) const fn decode(value: u64) -> Self {
        let major = ((value >> 8) & 0xfff) | ((value >> 32) & !0xfff);
        let minor = (value & 0xff) | ((value >> 12) & !0xff);
        Self {
            major: major as u32,
            minor: minor as u32,
        }
    }

    /// Linux `new_encode_dev` 的 64-bit `st_rdev` 编码。
    pub(crate) const fn encode(self) -> u64 {
        let major = self.major as u64;
        let minor = self.minor as u64;
        (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
    }
}
