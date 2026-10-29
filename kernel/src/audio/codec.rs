use super::state::MAX_BUFFER_FRAMES;
use crate::drivers::{PCM_BUFFER_FRAMES, PCM_PERIOD_FRAMES};

pub(crate) const SNDRV_PCM_IOCTL_PVERSION: usize = 0x8004_4100;
pub(crate) const SNDRV_PCM_IOCTL_TTSTAMP: usize = 0x4004_4103;
pub(crate) const SNDRV_PCM_IOCTL_USER_PVERSION: usize = 0x4004_4104;
pub(crate) const SNDRV_PCM_IOCTL_INFO: usize = 0x8120_4101;
pub(crate) const SNDRV_PCM_IOCTL_HW_REFINE: usize = 0xc260_4110;
pub(crate) const SNDRV_PCM_IOCTL_HW_PARAMS: usize = 0xc260_4111;
pub(crate) const SNDRV_PCM_IOCTL_HW_FREE: usize = 0x0000_4112;
pub(crate) const SNDRV_PCM_IOCTL_SW_PARAMS: usize = 0xc088_4113;
pub(crate) const SNDRV_PCM_IOCTL_STATUS: usize = 0x8098_4120;
pub(crate) const SNDRV_PCM_IOCTL_STATUS_EXT: usize = 0xc098_4124;
pub(crate) const SNDRV_PCM_IOCTL_DELAY: usize = 0x8008_4121;
pub(crate) const SNDRV_PCM_IOCTL_SYNC_PTR: usize = 0xc088_4123;
pub(crate) const SNDRV_PCM_IOCTL_PREPARE: usize = 0x0000_4140;
pub(crate) const SNDRV_PCM_IOCTL_START: usize = 0x0000_4142;
pub(crate) const SNDRV_PCM_IOCTL_DROP: usize = 0x0000_4143;
pub(crate) const SNDRV_PCM_IOCTL_WRITEI_FRAMES: usize = 0x4018_4150;

pub(super) const HW_PARAMS_BYTES: usize = 608;
pub(super) const PCM_INFO_BYTES: usize = 288;
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

/// `snd_pcm_hw_params` 里各参数的编号（Linux `SNDRV_PCM_HW_PARAM_*`）。
mod parameter {
    pub(super) const ACCESS: usize = 0;
    pub(super) const FORMAT: usize = 1;
    pub(super) const SUBFORMAT: usize = 2;
    pub(super) const SAMPLE_BITS: usize = 8;
    pub(super) const FRAME_BITS: usize = 9;
    pub(super) const CHANNELS: usize = 10;
    pub(super) const RATE: usize = 11;
    pub(super) const PERIOD_TIME: usize = 12;
    pub(super) const PERIOD_SIZE: usize = 13;
    pub(super) const PERIOD_BYTES: usize = 14;
    pub(super) const PERIODS: usize = 15;
    pub(super) const BUFFER_TIME: usize = 16;
    pub(super) const BUFFER_SIZE: usize = 17;
    pub(super) const BUFFER_BYTES: usize = 18;
    pub(super) const TICK_TIME: usize = 19;
}

const INTERVAL_OPEN_MIN: u32 = 1 << 0;
const INTERVAL_OPEN_MAX: u32 = 1 << 1;
const INTERVAL_INTEGER: u32 = 1 << 2;
const INTERVAL_EMPTY: u32 = 1 << 3;
const MASK_BYTES: usize = 32;
/// `snd_pcm_hw_params` 里 `cmask` 的字节偏移。
const CMASK_OFFSET: usize = 516;

/// 参数 `parameter`（掩码类）在结构体里的字节偏移。
const fn mask_offset(parameter: usize) -> usize {
    4 + parameter * MASK_BYTES
}

/// 参数 `parameter`（区间类，8..=19）在结构体里的字节偏移。
const fn interval_offset(parameter: usize) -> usize {
    260 + (parameter - 8) * 12
}

/// 指定软件 period 数时的配置：各区间及派生时间量的开端点标记。
///
/// 时间量（微秒）由采样率与帧数推出，不是整数：period_time = 256 / 48000 s = 5333.33 µs。Linux 用
/// 开区间端点 `(5333, 5334)` 表示"介于这两个整数之间"，`alsa-lib` 依赖这个形式才能正确折算。
const fn capability(parameter: usize, periods: u32) -> Option<(u32, u32, u32)> {
    let frames = periods * PCM_PERIOD_FRAMES as u32;
    let scaled_time = frames as u64 * 1_000_000;
    let buffer_time = (scaled_time / 48_000) as u32;
    let time = if scaled_time.is_multiple_of(48_000) {
        (buffer_time, buffer_time, INTERVAL_INTEGER)
    } else {
        (
            buffer_time,
            buffer_time + 1,
            INTERVAL_OPEN_MIN | INTERVAL_OPEN_MAX,
        )
    };
    // (min, max, flags)
    Some(match parameter {
        parameter::SAMPLE_BITS => (32, 32, INTERVAL_INTEGER),
        parameter::FRAME_BITS => (64, 64, INTERVAL_INTEGER),
        parameter::CHANNELS => (2, 2, INTERVAL_INTEGER),
        parameter::RATE => (48_000, 48_000, INTERVAL_INTEGER),
        parameter::PERIOD_TIME => (5_333, 5_334, INTERVAL_OPEN_MIN | INTERVAL_OPEN_MAX),
        parameter::PERIOD_SIZE => (256, 256, INTERVAL_INTEGER),
        parameter::PERIOD_BYTES => (2_048, 2_048, INTERVAL_INTEGER),
        parameter::PERIODS => (periods, periods, INTERVAL_INTEGER),
        parameter::BUFFER_TIME => time,
        parameter::BUFFER_SIZE => (frames, frames, INTERVAL_INTEGER),
        parameter::BUFFER_BYTES => (frames * 8, frames * 8, INTERVAL_INTEGER),
        parameter::TICK_TIME => (0, 0, INTERVAL_INTEGER),
        _ => return None,
    })
}

fn intersects(request: (u32, u32, u32), capability: (u32, u32, u32)) -> bool {
    let (min, max, flags) = request;
    let (low, high, supported) = capability;
    if flags & INTERVAL_EMPTY != 0 || min > max || flags & INTERVAL_INTEGER != 0 && low != high {
        return false;
    }
    let first = min.max(low);
    let last = max.min(high);
    first < last
        || first == last
            && !(first == min && flags & INTERVAL_OPEN_MIN != 0
                || first == max && flags & INTERVAL_OPEN_MAX != 0
                || first == low && supported & INTERVAL_OPEN_MIN != 0
                || first == high && supported & INTERVAL_OPEN_MAX != 0)
}

/// 固定硬件能力里的掩码参数允许的取值位集合。
const fn allowed_mask(parameter: usize) -> u32 {
    match parameter {
        // MMAP_INTERLEAVED(0) 与 RW_INTERLEAVED(3)。
        parameter::ACCESS => (1 << 0) | (1 << 3),
        // FLOAT_LE(14)。
        parameter::FORMAT => 1 << 14,
        // STD(0)。
        parameter::SUBFORMAT => 1 << 0,
        _ => 0,
    }
}

/// 把一组 `snd_pcm_hw_params` 与 PCM 支持的配置求交，写回结果。
///
/// 这是 `SNDRV_PCM_IOCTL_HW_REFINE` 的语义：调用方（`alsa-lib` 的 hw/plug 插件）提出一个约束空间，内核把它
/// 收窄到真正支持的子集；与能力不相交就是 `EINVAL`。格式、采样率与 period 固定，软件 ring 可协商。
/// 枚举有界 period 数，并同时验证 buffer size/bytes/time，保证这些相关约束不会各自通过却彼此矛盾。
///
/// # Parameters
///
/// - `bytes`: 完整的 608 字节 `snd_pcm_hw_params`，原地改写。
///
/// # Returns
///
/// 成功返回 `Ok(())`；任一参数与能力不相交返回 `Err(())`，此时 `bytes` 的内容不保证有意义。
pub(super) fn refine_hardware(bytes: &mut [u8; HW_PARAMS_BYTES]) -> Result<(), ()> {
    let mut changed = 0u32;
    for parameter in [parameter::ACCESS, parameter::FORMAT, parameter::SUBFORMAT] {
        let offset = mask_offset(parameter);
        let mut words = [0u32; MASK_BYTES / 4];
        for (index, word) in words.iter_mut().enumerate() {
            *word = read_u32(bytes, offset + index * 4).ok_or(())?;
        }
        let allowed = allowed_mask(parameter);
        let narrowed = words[0] & allowed;
        // 掩码的高位字全部不属于本硬件（它们对应的格式/访问方式都不支持），一并清零。
        if narrowed == 0 {
            return Err(());
        }
        if narrowed != words[0] || words[1..].iter().any(|&word| word != 0) {
            changed |= 1 << parameter;
        }
        write_u32(bytes, offset, narrowed).ok_or(())?;
        for index in 1..words.len() {
            write_u32(bytes, offset + index * 4, 0).ok_or(())?;
        }
    }
    // 1. 只保留同时满足全部区间的配置；这里是配置冷路径，最多检查 29 × 12 个点，不分配。
    let mut first = None;
    let mut last = None;
    for periods in (PCM_BUFFER_FRAMES / PCM_PERIOD_FRAMES) as u32
        ..=(MAX_BUFFER_FRAMES / PCM_PERIOD_FRAMES) as u32
    {
        let mut fits = true;
        for parameter in parameter::SAMPLE_BITS..=parameter::TICK_TIME {
            let offset = interval_offset(parameter);
            let request = (
                read_u32(bytes, offset).ok_or(())?,
                read_u32(bytes, offset + 4).ok_or(())?,
                read_u32(bytes, offset + 8).ok_or(())?,
            );
            if !intersects(request, capability(parameter, periods).ok_or(())?) {
                fits = false;
                break;
            }
        }
        if fits {
            first.get_or_insert(periods);
            last = Some(periods);
        }
    }
    let (first, last) = (first.ok_or(())?, last.ok_or(())?);
    // 2. 投影剩余配置的区间包络；派生时间保留真实开端点，精确 buffer 请求仍会约束 period 数。
    for parameter in parameter::SAMPLE_BITS..=parameter::TICK_TIME {
        let (capability_min, _, lower_flags) = capability(parameter, first).ok_or(())?;
        let (_, capability_max, upper_flags) = capability(parameter, last).ok_or(())?;
        let capability_flags = lower_flags & INTERVAL_OPEN_MIN
            | upper_flags & INTERVAL_OPEN_MAX
            | if parameter != parameter::BUFFER_TIME || first == last {
                lower_flags & upper_flags & INTERVAL_INTEGER
            } else {
                0
            };
        let offset = interval_offset(parameter);
        let (minimum, maximum, flags) = (
            read_u32(bytes, offset).ok_or(())?,
            read_u32(bytes, offset + 4).ok_or(())?,
            read_u32(bytes, offset + 8).ok_or(())?,
        );
        let unchanged = (minimum, maximum, flags & !INTERVAL_INTEGER)
            == (
                capability_min,
                capability_max,
                capability_flags & !INTERVAL_INTEGER,
            );
        if !unchanged || flags & INTERVAL_INTEGER != capability_flags & INTERVAL_INTEGER {
            changed |= 1 << parameter;
        }
        write_u32(bytes, offset, capability_min).ok_or(())?;
        write_u32(bytes, offset + 4, capability_max).ok_or(())?;
        write_u32(bytes, offset + 8, capability_flags).ok_or(())?;
    }
    write_u32(bytes, CMASK_OFFSET, changed).ok_or(())
}

/// 按 Linux `snd_pcm_hw_params_choose` 从有效约束中选定配置。
///
/// # Parameters
/// - `bytes`: 原生 `snd_pcm_hw_params`；成功时写回选定值与变化掩码。
///
/// # Errors
/// 约束没有共同可行配置时返回错误。格式、rate 与 period 固定，因此只需选首个 access 与最大 buffer。
pub(super) fn choose_hardware(bytes: &mut [u8; HW_PARAMS_BYTES]) -> Result<(), ()> {
    refine_hardware(bytes)?;
    let mut changed = read_u32(bytes, CMASK_OFFSET).ok_or(())?;
    for parameter in parameter::ACCESS..=parameter::SUBFORMAT {
        let offset = mask_offset(parameter);
        let mask = read_u32(bytes, offset).ok_or(())?;
        let first = mask.isolate_lowest_one();
        if first != mask {
            write_u32(bytes, offset, first).ok_or(())?;
            changed |= 1 << parameter;
        }
    }
    let offset = interval_offset(parameter::BUFFER_SIZE);
    let maximum = read_u32(bytes, offset + 4).ok_or(())?;
    if read_u32(bytes, offset) != Some(maximum) {
        write_u32(bytes, offset, maximum).ok_or(())?;
        changed |= 1 << parameter::BUFFER_SIZE;
    }
    write_u32(bytes, CMASK_OFFSET, changed).ok_or(())?;
    refine_hardware(bytes)
}

/// `snd_pcm_info`：PCM 设备与子流的身份和容量。
pub(super) struct PcmInfo<'a> {
    /// 本次 open 占用的子流编号，`snd_pcm_info.subdevice`。
    pub(super) subdevice: u32,
    /// 声卡的子流总数。
    pub(super) subdevices_count: u32,
    /// 仍空闲的子流数。
    pub(super) subdevices_available: u32,
    /// 声卡的 id 与名字、PCM 的 id 与名字。
    pub(super) card_id: &'a [u8],
    pub(super) name: &'a [u8],
}

/// 把 [`PcmInfo`] 编码成 288 字节 `snd_pcm_info`。
///
/// 字符串字段按 Linux 的固定宽度零填充写入，过长则截断并保证以 NUL 结尾。`dev_class`（`SNDRV_PCM_CLASS_GENERIC`
/// = 0）与 `dev_subclass`（`SNDRV_PCM_SUBCLASS_GENERIC_MIX` = 0）表示"通用、软件混音的多子流设备"。
pub(super) fn encode_pcm_info(info: &PcmInfo<'_>) -> [u8; PCM_INFO_BYTES] {
    fn text(bytes: &mut [u8], offset: usize, width: usize, value: &[u8]) {
        let length = value.len().min(width - 1);
        bytes[offset..offset + length].copy_from_slice(&value[..length]);
    }
    let mut bytes = [0u8; PCM_INFO_BYTES];
    // device = 0；stream = SNDRV_PCM_STREAM_PLAYBACK = 0；card = 0。三者都是 0，不需要写。
    let _ = write_u32(&mut bytes, 4, info.subdevice);
    text(&mut bytes, 16, 64, info.card_id);
    text(&mut bytes, 80, 80, info.name);
    // subname：Linux 的约定是 "subdevice #N"。
    let mut subname = [0u8; 32];
    let prefix = b"subdevice #";
    subname[..prefix.len()].copy_from_slice(prefix);
    let mut digits = [0u8; 10];
    let mut value = info.subdevice;
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for (offset, digit) in digits[..count].iter().rev().enumerate() {
        subname[prefix.len() + offset] = *digit;
    }
    text(&mut bytes, 160, 32, &subname[..prefix.len() + count]);
    let _ = write_u32(&mut bytes, 200, info.subdevices_count);
    let _ = write_u32(&mut bytes, 204, info.subdevices_available);
    bytes
}
