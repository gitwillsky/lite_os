use super::{engine::PcmEndpoint, state::MAX_BUFFER_PERIODS};
use crate::devices::model::{PCM_RATES, PcmCapabilities, PcmConfiguration, PcmFormat};

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

/// ALSA format 编号；只转换布局，不把总线格式号带入用户 ABI。
pub(super) const fn alsa_format(format: PcmFormat) -> u32 {
    match format {
        PcmFormat::S16 => 2,
        PcmFormat::S24Packed => 32,
        PcmFormat::S24 => 6,
        PcmFormat::S32 => 10,
        PcmFormat::Float => 14,
    }
}

pub(super) fn pcm_format(value: i32) -> Option<PcmFormat> {
    PcmFormat::ALL
        .into_iter()
        .find(|format| alsa_format(*format) as i32 == value)
}

fn duration(frames: u32, rate: u32) -> (u32, u32, u32) {
    let scaled = u64::from(frames) * 1_000_000;
    let low = (scaled / u64::from(rate)) as u32;
    if scaled.is_multiple_of(u64::from(rate)) {
        (low, low, INTERVAL_INTEGER)
    } else {
        (low, low + 1, INTERVAL_OPEN_MIN | INTERVAL_OPEN_MAX)
    }
}

fn capability(parameter: usize, configuration: PcmConfiguration, periods: u32) -> (u32, u32, u32) {
    let period = configuration.period_frames as u32;
    let frames = periods * period;
    let exact = |value| (value, value, INTERVAL_INTEGER);
    match parameter {
        parameter::SAMPLE_BITS => exact(configuration.format.bytes() as u32 * 8),
        parameter::FRAME_BITS => exact(configuration.frame_bytes() as u32 * 8),
        parameter::CHANNELS => exact(2),
        parameter::RATE => exact(configuration.rate),
        parameter::PERIOD_TIME => duration(period, configuration.rate),
        parameter::PERIOD_SIZE => exact(period),
        parameter::PERIOD_BYTES => exact(configuration.period_bytes() as u32),
        parameter::PERIODS => exact(periods),
        parameter::BUFFER_TIME => duration(frames, configuration.rate),
        parameter::BUFFER_SIZE => exact(frames),
        parameter::BUFFER_BYTES => exact(frames * configuration.frame_bytes() as u32),
        _ => exact(0),
    }
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

fn read_mask(bytes: &[u8], parameter: usize) -> Option<u64> {
    read_u64(bytes, mask_offset(parameter))
}

fn store_mask(bytes: &mut [u8], parameter: usize, value: u64) -> Option<bool> {
    let offset = mask_offset(parameter);
    let changed = bytes[offset..offset + MASK_BYTES]
        .iter()
        .enumerate()
        .any(|(index, byte)| {
            *byte
                != if index < 8 {
                    (value >> (index * 8)) as u8
                } else {
                    0
                }
        });
    bytes[offset..offset + MASK_BYTES].fill(0);
    write_u64(bytes, offset, value)?;
    Some(changed)
}

/// 按实际设备能力和 PCM 端点联合求交格式、采样率、period 与 buffer 约束。
///
/// # Parameters
/// - `bytes`: 原生 `snd_pcm_hw_params`，成功时写回约束与变化掩码。
/// - `supported`: adapter discovery 的能力快照。
/// - `endpoint`: Shared 只接受桌面正规形；Direct 直接协商设备支持的源格式。
///
/// # Errors
/// 没有共同可行配置时返回错误；不以重采样或截位替代不可用的原生格式。
pub(super) fn refine_hardware(
    bytes: &mut [u8; HW_PARAMS_BYTES],
    supported: PcmCapabilities,
    endpoint: PcmEndpoint,
) -> Result<(), ()> {
    let supported = if endpoint == PcmEndpoint::Shared {
        if supported.supports(PcmConfiguration::SHARED) {
            PcmCapabilities::SHARED
        } else {
            PcmCapabilities::EMPTY
        }
    } else {
        supported
    };
    let mut changed = read_u32(bytes, CMASK_OFFSET).ok_or(())?;
    for (parameter, allowed) in [(parameter::ACCESS, 0b1001), (parameter::SUBFORMAT, 1)] {
        let value = read_mask(bytes, parameter).ok_or(())? & allowed;
        if value == 0 {
            return Err(());
        }
        if store_mask(bytes, parameter, value).ok_or(())? {
            changed |= 1 << parameter;
        }
    }
    let requested_formats = read_mask(bytes, parameter::FORMAT).ok_or(())?;
    let mut requests = [(0, 0, 0); 12];
    for (index, request) in requests.iter_mut().enumerate() {
        let offset = interval_offset(parameter::SAMPLE_BITS + index);
        *request = (
            read_u32(bytes, offset).ok_or(())?,
            read_u32(bytes, offset + 4).ok_or(())?,
            read_u32(bytes, offset + 8).ok_or(())?,
        );
    }
    // 有界冷路径枚举；各投影取所有可行配置的包络，不能把非连续能力误当作连续采样率。
    let mut projected: [Option<(u32, u32, u32)>; 12] = [None; 12];
    let mut formats = 0u64;
    for format in PcmFormat::ALL {
        if requested_formats & (1 << alsa_format(format)) == 0 {
            continue;
        }
        for rate in PCM_RATES {
            let configuration = if endpoint == PcmEndpoint::Shared {
                if format != PcmFormat::Float || rate != 48_000 {
                    continue;
                }
                PcmConfiguration::SHARED
            } else {
                PcmConfiguration {
                    format,
                    rate,
                    period_frames: rate.div_ceil(200) as usize,
                }
            };
            if !supported.supports(configuration) {
                continue;
            }
            for periods in 4..=MAX_BUFFER_PERIODS as u32 {
                let fits = requests.iter().enumerate().all(|(index, request)| {
                    intersects(
                        *request,
                        capability(parameter::SAMPLE_BITS + index, configuration, periods),
                    )
                });
                if !fits {
                    continue;
                }
                formats |= 1 << alsa_format(format);
                for (index, projected) in projected.iter_mut().enumerate() {
                    let point = capability(parameter::SAMPLE_BITS + index, configuration, periods);
                    *projected = Some(match *projected {
                        None => point,
                        Some((low, high, flags)) => {
                            let (min, max, point_flags) = point;
                            let low_open = if min < low {
                                point_flags
                            } else if min > low {
                                flags
                            } else {
                                flags & point_flags
                            } & INTERVAL_OPEN_MIN;
                            let high_open = if max > high {
                                point_flags
                            } else if max < high {
                                flags
                            } else {
                                flags & point_flags
                            } & INTERVAL_OPEN_MAX;
                            (
                                low.min(min),
                                high.max(max),
                                low_open | high_open | (flags & point_flags & INTERVAL_INTEGER),
                            )
                        }
                    });
                }
            }
        }
    }
    if formats == 0 {
        return Err(());
    }
    if store_mask(bytes, parameter::FORMAT, formats).ok_or(())? {
        changed |= 1 << parameter::FORMAT;
    }
    for (index, interval) in projected.into_iter().enumerate() {
        let interval = interval.ok_or(())?;
        let parameter = parameter::SAMPLE_BITS + index;
        let offset = interval_offset(parameter);
        if requests[index] != interval {
            changed |= 1 << parameter;
        }
        write_u32(bytes, offset, interval.0).ok_or(())?;
        write_u32(bytes, offset + 4, interval.1).ok_or(())?;
        write_u32(bytes, offset + 8, interval.2).ok_or(())?;
    }
    write_u32(bytes, CMASK_OFFSET, changed).ok_or(())
}

/// 按 Linux `snd_pcm_hw_params_choose` 选首个 access/format/subformat、最低 rate、最大 buffer。
///
/// # Parameters
/// - `bytes`: 原生参数，成功时写回联合可行的单一配置。
/// - `supported`: 设备实际能力。
/// - `endpoint`: 共享或原生直出端点。
///
/// # Errors
/// 没有可行配置时返回错误。
pub(super) fn choose_hardware(
    bytes: &mut [u8; HW_PARAMS_BYTES],
    supported: PcmCapabilities,
    endpoint: PcmEndpoint,
) -> Result<(), ()> {
    refine_hardware(bytes, supported, endpoint)?;
    for parameter in parameter::ACCESS..=parameter::SUBFORMAT {
        let value = read_mask(bytes, parameter).ok_or(())?.isolate_lowest_one();
        if store_mask(bytes, parameter, value).ok_or(())? {
            let changed = read_u32(bytes, CMASK_OFFSET).ok_or(())? | (1 << parameter);
            write_u32(bytes, CMASK_OFFSET, changed).ok_or(())?;
        }
        refine_hardware(bytes, supported, endpoint)?;
    }
    for parameter in [
        parameter::CHANNELS,
        parameter::RATE,
        parameter::PERIOD_SIZE,
        parameter::BUFFER_SIZE,
        parameter::TICK_TIME,
    ] {
        let offset = interval_offset(parameter);
        let selected = read_u32(
            bytes,
            offset + usize::from(parameter == parameter::BUFFER_SIZE) * 4,
        )
        .ok_or(())?;
        write_u32(bytes, offset, selected).ok_or(())?;
        write_u32(bytes, offset + 4, selected).ok_or(())?;
        write_u32(bytes, offset + 8, INTERVAL_INTEGER).ok_or(())?;
        let changed = read_u32(bytes, CMASK_OFFSET).ok_or(())? | (1 << parameter);
        write_u32(bytes, CMASK_OFFSET, changed).ok_or(())?;
        refine_hardware(bytes, supported, endpoint)?;
    }
    Ok(())
}

/// `snd_pcm_info`：PCM 设备与子流的身份和容量。
pub(super) struct PcmInfo<'a> {
    pub(super) card: u32,
    pub(super) device: u32,
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
    let _ = write_u32(&mut bytes, 0, info.device);
    let _ = write_u32(&mut bytes, 12, info.card);
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
