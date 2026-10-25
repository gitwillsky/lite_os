use std::{
    collections::VecDeque,
    fmt,
    num::NonZeroU32,
    os::fd::{OwnedFd, RawFd},
};

/// Wayland 单条消息的最大字节数（含 8 字节头），与 libwayland 的缓冲区一致。
pub const MAX_MESSAGE_SIZE: usize = 4096;
pub const HEADER_SIZE: usize = 8;

/// 非零对象 ID；0 在 wire 上表示"无对象"，用 `Option<ObjectId>` 表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(NonZeroU32);

impl ObjectId {
    /// `wl_display` 固定占用的对象 ID。
    pub const DISPLAY: ObjectId = ObjectId(NonZeroU32::MIN);

    pub fn new(id: u32) -> Option<Self> {
        NonZeroU32::new(id).map(Self)
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }
}

/// 24.8 有符号定点数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fixed(pub i32);

impl Fixed {
    pub const fn from_int(value: i32) -> Self {
        Self(value << 8)
    }

    /// 四舍五入到最接近的 1/256。
    pub fn from_f64(value: f64) -> Self {
        Self((value * 256.0).round() as i32)
    }

    pub fn to_f64(self) -> f64 {
        f64::from(self.0) / 256.0
    }

    /// 向零取整的整数部分。
    pub const fn to_int(self) -> i32 {
        self.0 / 256
    }
}

/// `wl_registry.bind` 这类没有静态接口的 `new_id`：线上依次是 interface、version、id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicNewId {
    pub interface: String,
    pub version: u32,
    pub id: ObjectId,
}

/// 消息体解码失败；对端发送了违反协议的数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// 参数比消息体长度更长。
    Truncated,
    /// 参数读完后消息体还有剩余字节。
    TrailingBytes,
    UnknownOpcode(u16),
    /// 字符串缺少结尾 NUL 或不是合法 UTF-8。
    InvalidString,
    /// 不允许为空的字符串或对象参数为 0。
    UnexpectedNull,
    InvalidEnum,
    /// 消息需要文件描述符，但队列已空。
    MissingFd,
}

/// 消息编码失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// 消息超过 [`MAX_MESSAGE_SIZE`]；已写入的部分被回滚。
    TooLarge,
}

macro_rules! display_error {
    ($name:ident { $($variant:pat => $text:expr),* $(,)? }) => {
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(match self { $($variant => $text),* })
            }
        }
        impl std::error::Error for $name {}
    };
}

display_error!(DecodeError {
    Self::Truncated => "message body is shorter than its arguments",
    Self::TrailingBytes => "message body has trailing bytes",
    Self::UnknownOpcode(_) => "unknown opcode",
    Self::InvalidString => "invalid string argument",
    Self::UnexpectedNull => "null in a non-nullable argument",
    Self::InvalidEnum => "enum value out of range",
    Self::MissingFd => "message expects a file descriptor that was not received",
});

display_error!(EncodeError {
    Self::TooLarge => "message exceeds the 4096 byte limit",
});

/// 消息体的顺序读取器。
pub struct Reader<'a> {
    body: &'a [u8],
    position: usize,
    fds: &'a mut VecDeque<OwnedFd>,
}

fn padded(length: usize) -> usize {
    length.next_multiple_of(4)
}

impl<'a> Reader<'a> {
    /// `fds` 是连接上按到达顺序排列的未消费描述符。
    pub fn new(body: &'a [u8], fds: &'a mut VecDeque<OwnedFd>) -> Self {
        Self {
            body,
            position: 0,
            fds,
        }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.body.len())
            .ok_or(DecodeError::Truncated)?;
        let bytes = &self.body[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    pub fn uint(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn int(&mut self) -> Result<i32, DecodeError> {
        Ok(self.uint()? as i32)
    }

    pub fn fixed(&mut self) -> Result<Fixed, DecodeError> {
        self.int().map(Fixed)
    }

    pub fn object(&mut self) -> Result<Option<ObjectId>, DecodeError> {
        self.uint().map(ObjectId::new)
    }

    pub fn new_id(&mut self) -> Result<ObjectId, DecodeError> {
        self.object()?.ok_or(DecodeError::UnexpectedNull)
    }

    /// 读取字符串；长度为 0 表示 `None`。
    pub fn string(&mut self) -> Result<Option<String>, DecodeError> {
        let length = self.uint()? as usize;
        if length == 0 {
            return Ok(None);
        }
        let padded_length = padded(length);
        let bytes = self.take(padded_length)?;
        let (text, terminator) = bytes[..length].split_at(length - 1);
        if terminator != [0] {
            return Err(DecodeError::InvalidString);
        }
        String::from_utf8(text.to_vec())
            .map(Some)
            .map_err(|_| DecodeError::InvalidString)
    }

    pub fn array(&mut self) -> Result<Vec<u8>, DecodeError> {
        let length = self.uint()? as usize;
        let bytes = self.take(padded(length))?;
        Ok(bytes[..length].to_vec())
    }

    pub fn dynamic_new_id(&mut self) -> Result<DynamicNewId, DecodeError> {
        Ok(DynamicNewId {
            interface: self.string()?.ok_or(DecodeError::UnexpectedNull)?,
            version: self.uint()?,
            id: self.new_id()?,
        })
    }

    pub fn fd(&mut self) -> Result<OwnedFd, DecodeError> {
        self.fds.pop_front().ok_or(DecodeError::MissingFd)
    }

    /// 确认参数恰好读完整个消息体。
    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.position == self.body.len() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }
}

/// 把消息追加到发送缓冲区，同时收集随消息发送的文件描述符。
///
/// 描述符只借用（`RawFd`）：调用者必须在 `sendmsg` 完成之前保持对应的 `OwnedFd` 存活。
#[derive(Default)]
pub struct Writer {
    bytes: Vec<u8>,
    fds: Vec<RawFd>,
    message_start: usize,
    fds_start: usize,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 待发送的字节。
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 与 [`Writer::bytes`] 一同以 `SCM_RIGHTS` 发送的描述符，按消息顺序排列。
    pub fn fds(&self) -> &[RawFd] {
        &self.fds
    }

    /// 发送成功后丢弃前 `bytes` 个字节与前 `fds` 个描述符。
    pub fn consume(&mut self, bytes: usize, fds: usize) {
        self.bytes.drain(..bytes);
        self.fds.drain(..fds);
        self.message_start = 0;
        self.fds_start = 0;
    }

    /// 开始一条消息：写入占位头，参数随后追加，由 [`Writer::end`] 回填大小。
    pub fn begin(&mut self, object: ObjectId, opcode: u16) {
        self.message_start = self.bytes.len();
        self.fds_start = self.fds.len();
        self.bytes.extend_from_slice(&object.get().to_le_bytes());
        self.bytes
            .extend_from_slice(&u32::from(opcode).to_le_bytes());
    }

    /// 结束当前消息并回填头里的大小；超过上限时回滚整条消息。
    pub fn end(&mut self) -> Result<(), EncodeError> {
        let size = self.bytes.len() - self.message_start;
        if size > MAX_MESSAGE_SIZE {
            self.bytes.truncate(self.message_start);
            self.fds.truncate(self.fds_start);
            return Err(EncodeError::TooLarge);
        }
        let header = &mut self.bytes[self.message_start + 4..self.message_start + 8];
        let opcode = u32::from_le_bytes(header.try_into().unwrap());
        header.copy_from_slice(&(((size as u32) << 16) | opcode).to_le_bytes());
        Ok(())
    }

    pub fn uint(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn int(&mut self, value: i32) {
        self.uint(value as u32);
    }

    pub fn fixed(&mut self, value: Fixed) {
        self.int(value.0);
    }

    pub fn object(&mut self, id: Option<ObjectId>) {
        self.uint(id.map_or(0, ObjectId::get));
    }

    pub fn string(&mut self, value: Option<&str>) {
        let Some(text) = value else {
            return self.uint(0);
        };
        self.uint(text.len() as u32 + 1);
        self.bytes.extend_from_slice(text.as_bytes());
        self.bytes
            .resize(self.bytes.len() + padded(text.len() + 1) - text.len(), 0);
    }

    pub fn array(&mut self, value: &[u8]) {
        self.uint(value.len() as u32);
        self.bytes.extend_from_slice(value);
        self.bytes
            .resize(self.bytes.len() + padded(value.len()) - value.len(), 0);
    }

    pub fn dynamic_new_id(&mut self, value: &DynamicNewId) {
        self.string(Some(&value.interface));
        self.uint(value.version);
        self.object(Some(value.id));
    }

    pub fn fd(&mut self, fd: RawFd) {
        self.fds.push(fd);
    }
}

/// 帧头不合法；流已无法同步，连接必须终止。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    ZeroObject,
    /// 大小小于头、不是 4 的倍数或超过 [`MAX_MESSAGE_SIZE`]。
    InvalidSize(usize),
}

display_error!(FrameError {
    Self::ZeroObject => "message addressed to object 0",
    Self::InvalidSize(_) => "invalid message size",
});

/// 一条完整的入站消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub object: ObjectId,
    pub opcode: u16,
    pub body: Vec<u8>,
}

/// 把字节流切成消息：任意切分的输入都得到相同的帧序列。
#[derive(Default)]
pub struct Framer {
    buffer: Vec<u8>,
    consumed: usize,
}

impl Framer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        // 已消费的前缀在追加前回收，缓冲区大小因此有界于"一条未完成消息 + 本次输入"。
        if self.consumed > 0 {
            self.buffer.drain(..self.consumed);
            self.consumed = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// 取出下一条完整消息；数据不足时返回 `Ok(None)`。
    ///
    /// # Errors
    ///
    /// 帧头非法。此后流不可恢复。
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        let pending = &self.buffer[self.consumed..];
        let Some(header) = pending.get(..HEADER_SIZE) else {
            return Ok(None);
        };
        let object = u32::from_le_bytes(header[..4].try_into().unwrap());
        let word = u32::from_le_bytes(header[4..].try_into().unwrap());
        let (size, opcode) = ((word >> 16) as usize, (word & 0xFFFF) as u16);
        let object = ObjectId::new(object).ok_or(FrameError::ZeroObject)?;
        if size < HEADER_SIZE || size % 4 != 0 || size > MAX_MESSAGE_SIZE {
            return Err(FrameError::InvalidSize(size));
        }
        let Some(message) = pending.get(..size) else {
            return Ok(None);
        };
        let frame = Frame {
            object,
            opcode,
            body: message[HEADER_SIZE..].to_vec(),
        };
        self.consumed += size;
        Ok(Some(frame))
    }
}
