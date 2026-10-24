//! ALSA control 元素、标准结构 codec 与每个打开句柄的事件/偏好状态。

use super::codec::{read_u32, read_u64, write_u32, write_u64};
use super::engine::SUBSTREAMS;
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

/// 固定 Master 两项与每个 PCM 子流的音量元素数量。
pub(super) const ELEMENT_COUNT: usize = 2 + SUBSTREAMS;
/// Linux `snd_ctl_event`：type、mask、64-byte elem id。
pub(super) const EVENT_BYTES: usize = 72;

/// 卡上唯一的逻辑控制身份；PCM 音量用 ALSA elem id 的 index 区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Element {
    MasterVolume,
    MasterSwitch,
    PcmVolume(usize),
}

impl Element {
    /// 从枚举位置构造控制身份；越过固定元素表返回 None。
    pub(super) fn at(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::MasterVolume),
            1 => Some(Self::MasterSwitch),
            index if index < ELEMENT_COUNT => Some(Self::PcmVolume(index - 2)),
            _ => None,
        }
    }

    fn numid(self) -> u32 {
        match self {
            Self::MasterVolume => 1,
            Self::MasterSwitch => 2,
            Self::PcmVolume(index) => index as u32 + 3,
        }
    }

    fn name(self) -> &'static [u8] {
        match self {
            Self::MasterVolume => b"Master Playback Volume",
            Self::MasterSwitch => b"Master Playback Switch",
            Self::PcmVolume(_) => b"PCM Playback Volume",
        }
    }

    fn kind(self) -> u32 {
        if self == Self::MasterSwitch {
            TYPE_BOOLEAN
        } else {
            TYPE_INTEGER
        }
    }
}

/// 控制 ID 不存在，或结构/取值非法；由 DeviceFile 映射成 ENOENT/EINVAL。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ControlError {
    NoSuchElement,
    Invalid,
}

/// 解析 snd_ctl_elem_id；numid 非零时按 numid，否则按完整 ALSA 名字身份查找。
///
/// # Errors
///
/// 短结构为 Invalid；未知 ID、iface、device、subdevice、name 或 index 为 NoSuchElement。
pub(super) fn element_by_id(bytes: &[u8]) -> Result<Element, ControlError> {
    let numid = read_u32(bytes, 0).ok_or(ControlError::Invalid)?;
    if numid != 0 {
        return Element::at(numid as usize - 1).ok_or(ControlError::NoSuchElement);
    }
    if read_u32(bytes, 4) != Some(IFACE_MIXER)
        || read_u32(bytes, 8) != Some(0)
        || read_u32(bytes, 12) != Some(0)
    {
        return Err(ControlError::NoSuchElement);
    }
    let name = bytes.get(16..60).ok_or(ControlError::Invalid)?;
    let length = name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(name.len());
    let index = read_u32(bytes, 60).ok_or(ControlError::Invalid)? as usize;
    (0..ELEMENT_COUNT)
        .filter_map(Element::at)
        .find(|element| {
            element.name() == &name[..length]
                && match element {
                    Element::PcmVolume(slot) => *slot == index,
                    _ => index == 0,
                }
        })
        .ok_or(ControlError::NoSuchElement)
}

fn write_id(bytes: &mut [u8], element: Element) {
    bytes[..ELEM_ID_BYTES].fill(0);
    let _ = write_u32(bytes, 0, element.numid());
    let _ = write_u32(bytes, 4, IFACE_MIXER);
    bytes[16..16 + element.name().len()].copy_from_slice(element.name());
    if let Element::PcmVolume(index) = element {
        let _ = write_u32(bytes, 60, index as u32);
    }
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

/// 枚举固定控制表；按 list.offset/space 返回 IDs，同时写回 used/count。
///
/// # Errors
///
/// 结构字段不可读时返回 Invalid；输出容量始终不超过固定元素数量。
pub(super) fn list_elements(
    list: &mut [u8; ELEM_LIST_BYTES],
) -> Result<([u8; ELEM_ID_BYTES * ELEMENT_COUNT], usize), ControlError> {
    let offset = read_u32(list, 0).ok_or(ControlError::Invalid)? as usize;
    let space = read_u32(list, 4).ok_or(ControlError::Invalid)? as usize;
    let mut ids = [0u8; ELEM_ID_BYTES * ELEMENT_COUNT];
    let mut used = 0;
    for index in (offset..ELEMENT_COUNT).take(space) {
        write_id(
            &mut ids[used * ELEM_ID_BYTES..],
            Element::at(index).unwrap(),
        );
        used += 1;
    }
    let _ = write_u32(list, 8, used as u32);
    let _ = write_u32(list, 12, ELEMENT_COUNT as u32);
    Ok((ids, used))
}

/// 将元素的 canonical ID、类型、读写权限、双声道数与值域写回 INFO。
///
/// # Errors
///
/// 请求 ID 不存在或结构非法。
pub(super) fn describe_element(bytes: &mut [u8; ELEM_INFO_BYTES]) -> Result<(), ControlError> {
    let element = element_by_id(bytes)?;
    bytes.fill(0);
    write_id(bytes, element);
    write_u32(bytes, 64, element.kind()).ok_or(ControlError::Invalid)?;
    write_u32(bytes, 68, ACCESS_READWRITE).ok_or(ControlError::Invalid)?;
    write_u32(bytes, 72, 2).ok_or(ControlError::Invalid)?;
    if element.kind() == TYPE_INTEGER {
        write_u64(bytes, 88, u64::from(VOLUME_MAX)).ok_or(ControlError::Invalid)?;
        write_u64(bytes, 96, 1).ok_or(ControlError::Invalid)?;
    }
    Ok(())
}

/// 把 Engine 的双声道值编码进 ELEM_READ/WRITE 的 canonical value 结构。
///
/// # Errors
///
/// 请求 ID 不存在或结构非法；调用者提供已经过领域校验的 values。
pub(super) fn read_element(
    bytes: &mut [u8; ELEM_VALUE_BYTES],
    values: [u32; 2],
) -> Result<(), ControlError> {
    let element = element_by_id(bytes)?;
    write_id(bytes, element);
    for (channel, value) in values.into_iter().enumerate() {
        write_u64(bytes, 72 + channel * 8, u64::from(value)).ok_or(ControlError::Invalid)?;
    }
    Ok(())
}

/// 解码 ELEM_WRITE；校验完整的两个声道后才把领域值交给 Engine。
///
/// # Errors
///
/// ID 不存在或 volume 超过 100、switch 不是 0/1。这里不修改混音状态。
pub(super) fn write_element(
    bytes: &[u8; ELEM_VALUE_BYTES],
) -> Result<(Element, [u32; 2]), ControlError> {
    let element = element_by_id(bytes)?;
    let maximum = if element == Element::MasterSwitch {
        1
    } else {
        VOLUME_MAX
    };
    let mut values = [0; 2];
    for (channel, value) in values.iter_mut().enumerate() {
        let input = read_u64(bytes, 72 + channel * 8).ok_or(ControlError::Invalid)?;
        if input > u64::from(maximum) {
            return Err(ControlError::Invalid);
        }
        *value = input as u32;
    }
    Ok((element, values))
}

/// 编码一个标准 ELEM/VALUE 事件，保留完整 canonical ID。
pub(super) fn encode_event(element: Element) -> [u8; EVENT_BYTES] {
    let mut bytes = [0; EVENT_BYTES];
    let _ = write_u32(&mut bytes, 4, 1);
    write_id(&mut bytes[8..], element);
    bytes
}

/// 一个 control OFD 的订阅、合并事件队列与偏好子设备。
///
/// 队列最多包含每个固定元素一次，按首次变化 FIFO；同一元素未读前的变化合并。
/// closed 区分 actual fput 与 backend Arc observer，避免关闭后仍影响子流选择。
pub(super) struct ControlOpen {
    subscribed: bool,
    closed: bool,
    preferred: i32,
    pending: [Option<Element>; ELEMENT_COUNT],
    head: usize,
    len: usize,
}

impl ControlOpen {
    /// 新句柄未订阅，子设备自动选择。
    pub(super) fn new() -> Self {
        Self {
            subscribed: false,
            closed: false,
            preferred: -1,
            pending: [None; ELEMENT_COUNT],
            head: 0,
            len: 0,
        }
    }

    /// 句柄是否仍属于打开阶段。
    pub(super) fn active(&self) -> bool {
        !self.closed
    }
    /// 当前订阅状态，负值 SUBSCRIBE_EVENTS 查询使用此投影。
    pub(super) fn subscribed(&self) -> bool {
        !self.closed && self.subscribed
    }
    /// 幂等订阅/取消；再次订阅不丢弃未读事件，取消时清空队列。
    pub(super) fn subscribe(&mut self, enabled: bool) {
        self.subscribed = enabled && !self.closed;
        if !self.subscribed {
            self.pending.fill(None);
            self.head = 0;
            self.len = 0;
        }
    }
    /// 保存 Linux PCM_PREFER_SUBDEVICE 的原始 i32，选择策略在 PCM open 时裁决。
    pub(super) fn prefer(&mut self, preferred: i32) {
        self.preferred = preferred;
    }
    /// 返回仍活动句柄的偏好；关闭后的观察者不再影响 PCM open。
    pub(super) fn preferred(&self) -> i32 {
        if self.closed { -1 } else { self.preferred }
    }
    /// actual fput 撤销订阅和偏好；重复调用无副作用。
    pub(super) fn close(&mut self) {
        self.closed = true;
        self.subscribe(false);
    }
    /// 有可读的标准事件时为 true。
    pub(super) fn readable(&self) -> bool {
        self.subscribed() && self.len != 0
    }
    /// 合并或追加事件。无订阅时不积累历史；固定元素集使队列不需分配。
    pub(super) fn notify(&mut self, element: Element) {
        if !self.subscribed() || self.pending.contains(&Some(element)) {
            return;
        }
        let tail = (self.head + self.len) % ELEMENT_COUNT;
        self.pending[tail] = Some(element);
        self.len += 1;
    }
    /// 弹出最早的未读事件；调用者已预检用户目标内存。
    pub(super) fn pop(&mut self) -> Option<Element> {
        if !self.readable() {
            return None;
        }
        let element = self.pending[self.head].take();
        self.head = (self.head + 1) % ELEMENT_COUNT;
        self.len -= 1;
        element
    }
}
