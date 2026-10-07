pub(super) const CONTROL_QUEUE: u32 = 0;
pub(super) const EVENT_QUEUE: u32 = 1;
pub(super) const TX_QUEUE: u32 = 2;
pub(super) const RX_QUEUE: u32 = 3;

pub(super) const R_PCM_INFO: u32 = 0x0100;
pub(super) const R_PCM_SET_PARAMS: u32 = 0x0101;
pub(super) const R_PCM_PREPARE: u32 = 0x0102;
pub(super) const R_PCM_RELEASE: u32 = 0x0103;
pub(super) const R_PCM_START: u32 = 0x0104;
pub(super) const R_PCM_STOP: u32 = 0x0105;
pub(super) const EVT_PCM_XRUN: u32 = 0x1101;
pub(super) const S_OK: u32 = 0x8000;

pub(super) const D_OUTPUT: u8 = 0;
pub(super) const PCM_FMT_FLOAT: u8 = 19;
pub(super) const PCM_RATE_48000: u8 = 7;

pub(super) const PCM_INFO_BYTES: usize = 32;
pub(super) const CONTROL_REQUEST_BYTES: usize = 24;
pub(super) const CONTROL_RESPONSE_BYTES: usize = 36;
pub(super) const EVENT_BYTES: usize = 8;
pub(super) const XFER_BYTES: usize = 4;
pub(super) const STATUS_BYTES: usize = 8;

pub(super) fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Option<()> {
    bytes
        .get_mut(offset..offset + 4)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

pub(super) fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

pub(super) fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}
