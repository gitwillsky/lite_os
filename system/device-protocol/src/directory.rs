//! 设备目录：驱动宿主把每个设备的类、静态描述与通信句柄依次写入目录 channel，最后写一条结束标记。
//! 之后热插拔的设备（USB）沿同一 channel 继续发布。

use alloc::vec::Vec;

use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status};
use native_runtime::{Channel, Handle};

use crate::{Reader, Writer};

/// 设备类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Class {
    Block = 1,
    Network = 2,
    Display = 3,
    Input = 4,
    Pcm = 5,
    Port = 6,
    Entropy = 7,
    Usb = 8,
    /// 启动画面进度。
    Boot = 9,
    /// 启动时发现的设备已全部发布。
    EnumerationComplete = 10,
}

impl Class {
    fn decode(value: u32) -> Result<Self, Status> {
        Ok(match value {
            1 => Self::Block,
            2 => Self::Network,
            3 => Self::Display,
            4 => Self::Input,
            5 => Self::Pcm,
            6 => Self::Port,
            7 => Self::Entropy,
            8 => Self::Usb,
            9 => Self::Boot,
            10 => Self::EnumerationComplete,
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        })
    }
}

/// 一条目录记录。
pub struct Announcement {
    pub class: Class,
    /// 类相关的静态描述。
    pub description: Vec<u8>,
    /// 类相关的通信句柄（请求 channel、活动 Event、事件 channel）。
    pub handles: Vec<Handle>,
}

impl Announcement {
    /// 描述解码器。
    pub fn reader(&self) -> Reader<'_> {
        Reader::new(&self.description)
    }
}

/// 发布一条目录记录。
///
/// # Errors
///
/// 使用者已关闭目录或消息超限。
pub fn announce(directory: &Channel, class: Class, description: &Writer, handles: Vec<Handle>) -> Result<(), Status> {
    let mut message = Writer(Vec::new());
    message.u32(class as u32);
    message.0.extend_from_slice(description.as_bytes());
    directory.write(message.as_bytes(), handles)
}

/// 阻塞读取下一条目录记录。
///
/// # Errors
///
/// 驱动宿主已关闭目录，或记录格式错误。
pub fn receive(directory: &Channel) -> Result<Announcement, Status> {
    let message = directory.read_wait(DEADLINE_INFINITE)?;
    let mut reader = Reader::new(&message.bytes);
    let class = Class::decode(reader.u32()?)?;
    Ok(Announcement {
        class,
        description: message.bytes[4..].to_vec(),
        handles: message.handles,
    })
}

/// 描述编码器（不含请求头）。
pub fn description() -> Writer {
    Writer(Vec::new())
}
