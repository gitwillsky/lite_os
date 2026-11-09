//! GICv3 ITS 32-byte command codec；字段固定为小端，不保存第二份控制器状态。
#[derive(Clone, Copy)]
pub(super) struct Command(pub(super) [u64; 4]);
impl Command {
    pub(super) fn map_device(device: u32, table: u64, bits: u32, valid: bool) -> Self {
        Self([
            8 | u64::from(device) << 32,
            u64::from(bits - 1),
            table & 0x000f_ffff_ffff_ff00 | u64::from(valid) << 63,
            0,
        ])
    }
    pub(super) fn map_collection(collection: u16, target: u64) -> Self {
        Self([
            9,
            0,
            target & 0x000f_ffff_ffff_0000 | u64::from(collection) | 1 << 63,
            0,
        ])
    }
    pub(super) fn map_interrupt(device: u32, event: u32, interrupt: u32, collection: u16) -> Self {
        Self([
            10 | u64::from(device) << 32,
            u64::from(event) | u64::from(interrupt) << 32,
            u64::from(collection),
            0,
        ])
    }
    pub(super) fn move_interrupt(device: u32, event: u32, collection: u16) -> Self {
        Self([
            1 | u64::from(device) << 32,
            u64::from(event),
            u64::from(collection),
            0,
        ])
    }
    pub(super) fn invalidate(device: u32, event: u32) -> Self {
        Self([12 | u64::from(device) << 32, u64::from(event), 0, 0])
    }
    pub(super) fn clear(device: u32, event: u32) -> Self {
        Self([4 | u64::from(device) << 32, u64::from(event), 0, 0])
    }
    pub(super) fn discard(device: u32, event: u32) -> Self {
        Self([15 | u64::from(device) << 32, u64::from(event), 0, 0])
    }
    pub(super) fn synchronize(target: u64) -> Self {
        Self([5, 0, target & 0x000f_ffff_ffff_0000, 0])
    }
}
