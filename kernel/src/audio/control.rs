//! ALSA 控制接口的纯协议层：元素表、`snd_ctl_*` 结构的编解码与主音量的读写语义。
//!
//! 声卡只有两个固定元素——`Master Playback Volume` 与 `Master Playback Switch`，所以元素表是常量而不是可注册
//! 的动态集合。本文件不碰设备文件、锁或引擎：输入是字节与一个 [`Master`] 值，输出是字节与新的 [`Master`]，
//! 因此可以在宿主上逐字节核对 Linux 7.1 的 ABI。

use super::codec::{read_u32, read_u64, write_u32, write_u64};
use super::engine::Master;
use super::mixer::VOLUME_MAX;

/// `SNDRV_CTL_VERSION`：控制协议版本 2.0.9。
pub(super) const CTL_PROTOCOL_VERSION: i32 = 0x0002_0009;

pub(crate) const SNDRV_CTL_IOCTL_PVERSION: usize = 0x8004_5500;
pub(crate) const SNDRV_CTL_IOCTL_CARD_INFO: usize = 0x8178_5501;
pub(crate) const SNDRV_CTL_IOCTL_ELEM_LIST: usize = 0xc050_5510;
pub(crate) const SNDRV_CTL_IOCTL_ELEM_INFO: usize = 0xc110_5511;
pub(crate) const SNDRV_CTL_IOCTL_ELEM_READ: usize = 0xc4c8_5512;
pub(crate) const SNDRV_CTL_IOCTL_ELEM_WRITE: usize = 0xc4c8_5513;
pub(crate) const SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS: usize = 0xc004_5516;
pub(crate) const SNDRV_CTL_IOCTL_PCM_NEXT_DEVICE: usize = 0x8004_5530;
pub(crate) const SNDRV_CTL_IOCTL_PCM_INFO: usize = 0xc120_5531;
pub(crate) const SNDRV_CTL_IOCTL_PCM_PREFER_SUBDEVICE: usize = 0x4004_5532;

pub(super) const CARD_INFO_BYTES: usize = 376;
pub(super) const ELEM_ID_BYTES: usize = 64;
pub(super) const ELEM_LIST_BYTES: usize = 80;
pub(super) const ELEM_INFO_BYTES: usize = 272;
pub(super) const ELEM_VALUE_BYTES: usize = 1224;

const IFACE_MIXER: u32 = 2;
const TYPE_BOOLEAN: u32 = 1;
const TYPE_INTEGER: u32 = 2;
/// `SNDRV_CTL_ELEM_ACCESS_READWRITE`。
const ACCESS_READWRITE: u32 = 0x3;

/// 元素的静态描述。
struct Element {
    numid: u32,
    name: &'static [u8],
    kind: u32,
}

const ELEMENTS: [Element; 2] = [
    Element {
        numid: 1,
        name: b"Master Playback Volume",
        kind: TYPE_INTEGER,
    },
    Element {
        numid: 2,
        name: b"Master Playback Switch",
        kind: TYPE_BOOLEAN,
    },
];

/// 控制操作的失败原因，由调用方映射成 errno。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ControlError {
    /// 没有这个元素（`ENOENT`）。
    NoSuchElement,
    /// 值越界或字节不是合法结构（`EINVAL`）。
    Invalid,
}

fn element_by_id(bytes: &[u8]) -> Result<&'static Element, ControlError> {
    let numid = read_u32(bytes, 0).ok_or(ControlError::Invalid)?;
    if numid != 0 {
        return ELEMENTS
            .iter()
            .find(|element| element.numid == numid)
            .ok_or(ControlError::NoSuchElement);
    }
    // numid 为 0 时按 (iface, device, subdevice, name, index) 查找，`alsa-lib` 打开混音器时就是这样定位元素的。
    let name = bytes.get(16..60).ok_or(ControlError::Invalid)?;
    let length = name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(name.len());
    let index = read_u32(bytes, 60).ok_or(ControlError::Invalid)?;
    if read_u32(bytes, 4) != Some(IFACE_MIXER) || index != 0 {
        return Err(ControlError::NoSuchElement);
    }
    ELEMENTS
        .iter()
        .find(|element| element.name == &name[..length])
        .ok_or(ControlError::NoSuchElement)
}

fn write_id(bytes: &mut [u8], element: &Element) {
    let _ = write_u32(bytes, 0, element.numid);
    let _ = write_u32(bytes, 4, IFACE_MIXER);
    bytes[16..16 + element.name.len()].copy_from_slice(element.name);
}

/// `snd_ctl_card_info` 的文本字段。
pub(super) struct CardText<'a> {
    pub(super) id: &'a [u8],
    pub(super) name: &'a [u8],
    pub(super) long_name: &'a [u8],
    pub(super) mixer_name: &'a [u8],
}

/// 编码 376 字节 `snd_ctl_card_info`：卡号 0，驱动名 `LiteOS`。
pub(super) fn encode_card_info(text: &CardText<'_>) -> [u8; CARD_INFO_BYTES] {
    fn put(bytes: &mut [u8], offset: usize, width: usize, value: &[u8]) {
        let length = value.len().min(width - 1);
        bytes[offset..offset + length].copy_from_slice(&value[..length]);
    }
    let mut bytes = [0u8; CARD_INFO_BYTES];
    // card = 0（偏移 0）已经是零。
    put(&mut bytes, 8, 16, text.id);
    put(&mut bytes, 24, 16, b"LiteOS");
    put(&mut bytes, 40, 32, text.name);
    put(&mut bytes, 72, 80, text.long_name);
    put(&mut bytes, 168, 80, text.mixer_name);
    bytes
}

/// 处理 `ELEM_LIST`：把元素表从 `offset` 起的 id 写进调用方提供的数组。
///
/// 结构：`offset @0`、`space @4`（调用方数组可放多少个 id）、`used @8`（写出多少个）、`count @12`
/// （元素总数）、`pids @16`（用户数组指针）。返回要写入 `pids` 的 id 序列；`list` 里的 `used`/`count` 原地更新。
pub(super) fn list_elements(
    list: &mut [u8; ELEM_LIST_BYTES],
) -> Result<([u8; ELEM_ID_BYTES * 2], usize), ControlError> {
    let offset = read_u32(list, 0).ok_or(ControlError::Invalid)? as usize;
    let space = read_u32(list, 4).ok_or(ControlError::Invalid)? as usize;
    let mut ids = [0u8; ELEM_ID_BYTES * 2];
    let mut used = 0;
    for element in ELEMENTS.iter().skip(offset).take(space) {
        write_id(
            &mut ids[used * ELEM_ID_BYTES..(used + 1) * ELEM_ID_BYTES],
            element,
        );
        used += 1;
    }
    let _ = write_u32(list, 8, used as u32);
    let _ = write_u32(list, 12, ELEMENTS.len() as u32);
    Ok((ids, used))
}

/// 处理 `ELEM_INFO`：按 id 填写元素的类型、读写权限、声道数与取值范围。
pub(super) fn describe_element(bytes: &mut [u8; ELEM_INFO_BYTES]) -> Result<(), ControlError> {
    let element = element_by_id(&bytes[..ELEM_ID_BYTES])?;
    write_id(&mut bytes[..ELEM_ID_BYTES], element);
    write_u32(bytes, 64, element.kind).ok_or(ControlError::Invalid)?;
    write_u32(bytes, 68, ACCESS_READWRITE).ok_or(ControlError::Invalid)?;
    write_u32(bytes, 72, 2).ok_or(ControlError::Invalid)?;
    if element.kind == TYPE_INTEGER {
        write_u64(bytes, 80, 0).ok_or(ControlError::Invalid)?;
        write_u64(bytes, 88, u64::from(VOLUME_MAX)).ok_or(ControlError::Invalid)?;
        write_u64(bytes, 96, 1).ok_or(ControlError::Invalid)?;
    }
    Ok(())
}

/// 处理 `ELEM_READ`：把当前值写进 `value`（偏移 72 起，每个声道一个 `long`，即 8 字节）。
pub(super) fn read_element(
    bytes: &mut [u8; ELEM_VALUE_BYTES],
    master: Master,
) -> Result<(), ControlError> {
    let element = element_by_id(&bytes[..ELEM_ID_BYTES])?;
    for channel in 0..2 {
        let value = if element.kind == TYPE_INTEGER {
            u64::from(master.volume[channel])
        } else {
            u64::from(master.enabled[channel])
        };
        write_u64(bytes, 72 + channel * 8, value).ok_or(ControlError::Invalid)?;
    }
    Ok(())
}

/// 处理 `ELEM_WRITE`：校验并应用新值。
///
/// # Returns
///
/// 新的 [`Master`] 与"值是否发生变化"。Linux 约定变化时 ioctl 返回 1、未变返回 0，`alsa-lib` 靠它决定是否
/// 发出变化通知。
///
/// # Errors
///
/// 元素不存在，或任一声道的值超出该元素的合法范围（音量 0..=100，开关 0/1）。
pub(super) fn write_element(
    bytes: &[u8; ELEM_VALUE_BYTES],
    master: Master,
) -> Result<(Master, bool), ControlError> {
    let element = element_by_id(&bytes[..ELEM_ID_BYTES])?;
    let mut next = master;
    for channel in 0..2 {
        let value = read_u64(bytes, 72 + channel * 8).ok_or(ControlError::Invalid)?;
        if element.kind == TYPE_INTEGER {
            if value > u64::from(VOLUME_MAX) {
                return Err(ControlError::Invalid);
            }
            next.volume[channel] = value as u32;
        } else {
            if value > 1 {
                return Err(ControlError::Invalid);
            }
            next.enabled[channel] = value == 1;
        }
    }
    Ok((next, next != master))
}
