pub(crate) const SNDRV_PCM_IOCTL_PVERSION: usize = 0x8004_4100;
pub(crate) const SNDRV_PCM_IOCTL_HW_PARAMS: usize = 0xc260_4111;
pub(crate) const SNDRV_PCM_IOCTL_HW_FREE: usize = 0x0000_4112;
pub(crate) const SNDRV_PCM_IOCTL_SW_PARAMS: usize = 0xc088_4113;
pub(crate) const SNDRV_PCM_IOCTL_STATUS: usize = 0x8098_4120;
pub(crate) const SNDRV_PCM_IOCTL_DELAY: usize = 0x8008_4121;
pub(crate) const SNDRV_PCM_IOCTL_SYNC_PTR: usize = 0xc088_4123;
pub(crate) const SNDRV_PCM_IOCTL_PREPARE: usize = 0x0000_4140;
pub(crate) const SNDRV_PCM_IOCTL_START: usize = 0x0000_4142;
pub(crate) const SNDRV_PCM_IOCTL_DROP: usize = 0x0000_4143;
pub(crate) const SNDRV_PCM_IOCTL_WRITEI_FRAMES: usize = 0x4018_4150;

pub(super) const HW_PARAMS_BYTES: usize = 608;
pub(super) const SW_PARAMS_BYTES: usize = 136;
pub(super) const STATUS_BYTES: usize = 152;
pub(super) const XFER_BYTES: usize = 24;
pub(super) const SYNC_PTR_BYTES: usize = 136;
pub(super) const PCM_PROTOCOL_VERSION: i32 = 0x0002_0012;

pub(super) fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

pub(super) fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

pub(super) fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Option<()> {
    bytes
        .get_mut(offset..offset + 4)?
        .copy_from_slice(&value.to_ne_bytes());
    Some(())
}

pub(super) fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> Option<()> {
    bytes
        .get_mut(offset..offset + 8)?
        .copy_from_slice(&value.to_ne_bytes());
    Some(())
}

pub(super) fn exact_mask(bytes: &[u8], offset: usize) -> Option<i32> {
    let words = bytes.get(offset..offset + 32)?;
    let mut value = None;
    for (word_index, word) in words.as_chunks::<4>().0.iter().enumerate() {
        let word = u32::from_ne_bytes(*word);
        if word == 0 {
            continue;
        }
        if word.count_ones() != 1 || value.is_some() {
            return None;
        }
        value = Some((word_index * 32 + word.trailing_zeros() as usize) as i32);
    }
    value
}

pub(super) fn exact_interval(bytes: &[u8], parameter: usize) -> Option<u32> {
    let offset = 260 + parameter.checked_sub(8)? * 12;
    let minimum = read_u32(bytes, offset)?;
    let maximum = read_u32(bytes, offset + 4)?;
    let flags = read_u32(bytes, offset + 8)?;
    (minimum == maximum && flags & 0b1011 == 0).then_some(minimum)
}

/// Linux short-transfer policy: any prior progress wins over the later errno.
pub(super) fn stop_or_error(completed: usize, error: isize) -> Result<(), isize> {
    if completed == 0 { Err(error) } else { Ok(()) }
}
