//! 全部协议消息类型的 payload 定义与逐字段编解码。
//!
//! 每种定长消息提供 `encode(&self, buf)`（写完整帧）与 `parse(payload)`；
//! 变长消息（[`CreateSurface`] / [`SetTitle`] / [`Commit`]）为关联函数形式，
//! parse 时校验 `title_len` / `num_rects` 不越出 payload。

use crate::{
    CLOSE_REQUEST, COMMIT, CONFIGURE, CREATE_SURFACE, DESTROY_SURFACE, FOCUS, HELLO, INPUT_KEY,
    INPUT_POINTER, INPUT_SYNC_RESET, MAX_DAMAGE_RECTS, SET_BUFFER, SET_TITLE, SURFACE_CREATED,
    WELCOME, encode_frame, read_i32, read_u32, write_i32, write_u16, write_u32,
};

/// `HELLO`（C→S）：客户端连接后发送的第一条消息。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Hello {
    /// 客户端支持的协议版本，当前必须为 [`crate::PROTOCOL_VERSION`]。
    pub version: u32,
}

impl Hello {
    /// 编码为完整帧写入 `buf`，返回帧总长度；`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        let total = encode_frame(buf, HELLO, 4)?;
        write_u32(buf, 8, self.version)?;
        Some(total)
    }

    /// 从 payload（[`crate::parse_header`] 剥离头部后的切片）解析。
    ///
    /// 不足 4 字节返回 `None`；多余的尾部字节被忽略，便于前向兼容。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        Some(Self {
            version: read_u32(payload, 0)?,
        })
    }
}

/// `WELCOME`（S→C）：握手应答，经 `SCM_RIGHTS` 附带 `/dev/dri/card0` 的 dup fd，
/// 用 [`crate::send_message_with_fd`] / [`crate::recv_message`] 收发。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Welcome {
    /// 服务端确认的协议版本。
    pub version: u32,
}

impl Welcome {
    /// 编码为完整帧写入 `buf`，返回帧总长度；`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        let total = encode_frame(buf, WELCOME, 4)?;
        write_u32(buf, 8, self.version)?;
        Some(total)
    }

    /// 从 payload 解析；不足 4 字节返回 `None`，多余尾部字节被忽略。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        Some(Self {
            version: read_u32(payload, 0)?,
        })
    }
}

/// `CREATE_SURFACE`（C→S）：创建 surface，payload 后紧随 `title_len` 字节的
/// UTF-8 标题。
///
/// `gem_handle` 指向客户端已在共享 DRM fd 上 `CREATE_DUMB` 建好的 buffer，
/// 尺寸必须恰好为 `width` × `height`（XRGB8888）。**提及时 handle 所有权转移给
/// 桌面**：桌面负责最终 `DESTROY_DUMB`，客户端此后绝不销毁该 handle。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CreateSurface {
    /// surface 宽度（像素）。
    pub width: u32,
    /// surface 高度（像素）。
    pub height: u32,
    /// 客户端创建、随本条消息转移所有权给桌面的 GEM handle。
    pub gem_handle: u32,
    /// 标志位，见 [`crate::SURFACE_FLAG_UNDECORATED`]。
    pub flags: u32,
    /// 紧随其后的标题字节数。
    pub title_len: u32,
}

impl CreateSurface {
    /// 编码为完整帧（header + 定长字段 + `title` 字节）写入 `buf`，返回帧总长度；
    /// `buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(
        buf: &mut [u8],
        width: u32,
        height: u32,
        gem_handle: u32,
        flags: u32,
        title: &[u8],
    ) -> Option<usize> {
        let total = encode_frame(buf, CREATE_SURFACE, 20 + title.len())?;
        write_u32(buf, 8, width)?;
        write_u32(buf, 12, height)?;
        write_u32(buf, 16, gem_handle)?;
        write_u32(buf, 20, flags)?;
        write_u32(buf, 24, title.len() as u32)?;
        buf.get_mut(28..28 + title.len())?.copy_from_slice(title);
        Some(total)
    }

    /// 从 payload 解析，返回定长字段与标题字节切片。
    ///
    /// 定长部分不足 20 字节或 `title_len` 越出 payload 时返回 `None`。
    pub fn parse(payload: &[u8]) -> Option<(Self, &[u8])> {
        if payload.len() < 20 {
            return None;
        }
        let message = Self {
            width: read_u32(payload, 0)?,
            height: read_u32(payload, 4)?,
            gem_handle: read_u32(payload, 8)?,
            flags: read_u32(payload, 12)?,
            title_len: read_u32(payload, 16)?,
        };
        let title_len = message.title_len as usize;
        let title = payload.get(20..20usize.checked_add(title_len)?)?;
        Some((message, title))
    }
}

/// `SURFACE_CREATED`（S→C）：[`CREATE_SURFACE`](crate::CREATE_SURFACE) 的应答。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SurfaceCreated {
    /// 分配到的 surface id；`error` 非 0 时无意义。
    pub surface_id: u32,
    /// 0 表示成功，否则为 errno。
    pub error: u32,
}

impl SurfaceCreated {
    /// 编码为完整帧写入 `buf`，返回帧总长度；`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        let total = encode_frame(buf, SURFACE_CREATED, 8)?;
        write_u32(buf, 8, self.surface_id)?;
        write_u32(buf, 12, self.error)?;
        Some(total)
    }

    /// 从 payload 解析；不足 8 字节返回 `None`，多余尾部字节被忽略。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 8 {
            return None;
        }
        Some(Self {
            surface_id: read_u32(payload, 0)?,
            error: read_u32(payload, 4)?,
        })
    }
}

/// `COMMIT`（C→S）：提交 damage，payload 后紧随 `num_rects` 个 `[u16; 4]`
/// clip（`x1, y1, x2, y2` 半开矩形，surface 内容相对坐标）。
///
/// `num_rects` 为 0 表示整幅 surface 均有 damage；上限 [`MAX_DAMAGE_RECTS`]。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Commit {
    /// 目标 surface id。
    pub surface_id: u32,
    /// 紧随其后的 clip 数量。
    pub num_rects: u32,
}

impl Commit {
    /// 编码为完整帧（header + 定长字段 + `rects` 逐个 clip）写入 `buf`，返回帧总长度。
    ///
    /// `rects` 超过 [`MAX_DAMAGE_RECTS`]、`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(buf: &mut [u8], surface_id: u32, rects: &[[u16; 4]]) -> Option<usize> {
        if rects.len() > MAX_DAMAGE_RECTS {
            return None;
        }
        let total = encode_frame(buf, COMMIT, 8 + rects.len() * 8)?;
        write_u32(buf, 8, surface_id)?;
        write_u32(buf, 12, rects.len() as u32)?;
        for (index, rect) in rects.iter().enumerate() {
            for (field, value) in rect.iter().enumerate() {
                write_u16(buf, 16 + index * 8 + field * 2, *value)?;
            }
        }
        Some(total)
    }

    /// 从 payload 解析，返回定长字段与 clip 迭代器。
    ///
    /// 定长部分不足 8 字节、`num_rects` 超过 [`MAX_DAMAGE_RECTS`]
    /// 或 clip 字节越出 payload 时返回 `None`。
    pub fn parse(payload: &[u8]) -> Option<(Self, CommitRects<'_>)> {
        if payload.len() < 8 {
            return None;
        }
        let message = Self {
            surface_id: read_u32(payload, 0)?,
            num_rects: read_u32(payload, 4)?,
        };
        let num_rects = message.num_rects as usize;
        if num_rects > MAX_DAMAGE_RECTS {
            return None;
        }
        let rect_bytes = num_rects.checked_mul(8)?;
        if payload.len() < 8 + rect_bytes {
            return None;
        }
        let rects = CommitRects {
            payload: &payload[..8 + rect_bytes],
            index: 0,
            count: num_rects,
        };
        Some((message, rects))
    }
}

/// [`Commit::parse`] 返回的 damage clip 迭代器，逐个产出 `{x1, y1, x2, y2}` 半开矩形。
pub struct CommitRects<'a> {
    payload: &'a [u8],
    index: usize,
    count: usize,
}

impl Iterator for CommitRects<'_> {
    type Item = [u16; 4];

    fn next(&mut self) -> Option<[u16; 4]> {
        if self.index >= self.count {
            return None;
        }
        let base = 8 + self.index * 8;
        let mut rect = [0u16; 4];
        for (field, slot) in rect.iter_mut().enumerate() {
            let at = base + field * 2;
            *slot = u16::from_le_bytes(self.payload[at..at + 2].try_into().ok()?);
        }
        self.index += 1;
        Some(rect)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.count - self.index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for CommitRects<'_> {}

/// `INPUT_KEY`（S→C）：raw evdev `EV_KEY` 事件。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputKey {
    /// 目标 surface id。
    pub surface_id: u32,
    /// evdev 键码（`KEY_*`）。
    pub code: u32,
    /// 按键值：0 释放，1 按下，2 重复。
    pub value: i32,
}

impl InputKey {
    /// 编码为完整帧写入 `buf`，返回帧总长度；`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        let total = encode_frame(buf, INPUT_KEY, 12)?;
        write_u32(buf, 8, self.surface_id)?;
        write_u32(buf, 12, self.code)?;
        write_i32(buf, 16, self.value)?;
        Some(total)
    }

    /// 从 payload 解析；不足 12 字节返回 `None`，多余尾部字节被忽略。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 12 {
            return None;
        }
        Some(Self {
            surface_id: read_u32(payload, 0)?,
            code: read_u32(payload, 4)?,
            value: read_i32(payload, 8)?,
        })
    }
}

/// `INPUT_POINTER`（S→C）：指针事件，坐标为 surface 内容相对坐标。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputPointer {
    /// 目标 surface id。
    pub surface_id: u32,
    /// 指针 x 坐标（surface 内容相对）。
    pub x: u32,
    /// 指针 y 坐标（surface 内容相对）。
    pub y: u32,
    /// 按键位掩码：bit0 = left，bit1 = right，bit2 = middle。
    pub buttons: u32,
    /// 滚轮增量（垂直滚动步数，向上为正）。
    pub wheel: i32,
}

impl InputPointer {
    /// 编码为完整帧写入 `buf`，返回帧总长度；`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        let total = encode_frame(buf, INPUT_POINTER, 20)?;
        write_u32(buf, 8, self.surface_id)?;
        write_u32(buf, 12, self.x)?;
        write_u32(buf, 16, self.y)?;
        write_u32(buf, 20, self.buttons)?;
        write_i32(buf, 24, self.wheel)?;
        Some(total)
    }

    /// 从 payload 解析；不足 20 字节返回 `None`，多余尾部字节被忽略。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 20 {
            return None;
        }
        Some(Self {
            surface_id: read_u32(payload, 0)?,
            x: read_u32(payload, 4)?,
            y: read_u32(payload, 8)?,
            buttons: read_u32(payload, 12)?,
            wheel: read_i32(payload, 16)?,
        })
    }
}

/// `FOCUS`（S→C）：surface 键盘焦点变化。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Focus {
    /// 目标 surface id。
    pub surface_id: u32,
    /// 1 获得焦点，0 失去焦点。
    pub focused: u32,
}

impl Focus {
    /// 编码为完整帧写入 `buf`，返回帧总长度；`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        let total = encode_frame(buf, FOCUS, 8)?;
        write_u32(buf, 8, self.surface_id)?;
        write_u32(buf, 12, self.focused)?;
        Some(total)
    }

    /// 从 payload 解析；不足 8 字节返回 `None`，多余尾部字节被忽略。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 8 {
            return None;
        }
        Some(Self {
            surface_id: read_u32(payload, 0)?,
            focused: read_u32(payload, 4)?,
        })
    }
}

/// `CLOSE_REQUEST`（S→C）：桌面请求关闭该 surface（如用户点击关闭按钮），
/// 客户端收到后应退出。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CloseRequest {
    /// 目标 surface id。
    pub surface_id: u32,
}

impl CloseRequest {
    /// 编码为完整帧写入 `buf`，返回帧总长度；`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        let total = encode_frame(buf, CLOSE_REQUEST, 4)?;
        write_u32(buf, 8, self.surface_id)?;
        Some(total)
    }

    /// 从 payload 解析；不足 4 字节返回 `None`，多余尾部字节被忽略。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        Some(Self {
            surface_id: read_u32(payload, 0)?,
        })
    }
}

/// `SET_TITLE`（C→S）：更新 surface 标题，payload 后紧随 `title_len` 字节 UTF-8。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SetTitle {
    /// 目标 surface id。
    pub surface_id: u32,
    /// 紧随其后的标题字节数。
    pub title_len: u32,
}

impl SetTitle {
    /// 编码为完整帧（header + 定长字段 + `title` 字节）写入 `buf`，返回帧总长度；
    /// `buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(buf: &mut [u8], surface_id: u32, title: &[u8]) -> Option<usize> {
        let total = encode_frame(buf, SET_TITLE, 8 + title.len())?;
        write_u32(buf, 8, surface_id)?;
        write_u32(buf, 12, title.len() as u32)?;
        buf.get_mut(16..16 + title.len())?.copy_from_slice(title);
        Some(total)
    }

    /// 从 payload 解析，返回定长字段与标题字节切片。
    ///
    /// 定长部分不足 8 字节或 `title_len` 越出 payload 时返回 `None`。
    pub fn parse(payload: &[u8]) -> Option<(Self, &[u8])> {
        if payload.len() < 8 {
            return None;
        }
        let message = Self {
            surface_id: read_u32(payload, 0)?,
            title_len: read_u32(payload, 4)?,
        };
        let title_len = message.title_len as usize;
        let title = payload.get(8..8usize.checked_add(title_len)?)?;
        Some((message, title))
    }
}

/// `DESTROY_SURFACE`（C→S）：客户端主动销毁 surface。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DestroySurface {
    /// 目标 surface id。
    pub surface_id: u32,
}

impl DestroySurface {
    /// 编码为完整帧写入 `buf`，返回帧总长度；`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        let total = encode_frame(buf, DESTROY_SURFACE, 4)?;
        write_u32(buf, 8, self.surface_id)?;
        Some(total)
    }

    /// 从 payload 解析；不足 4 字节返回 `None`，多余尾部字节被忽略。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        Some(Self {
            surface_id: read_u32(payload, 0)?,
        })
    }
}

/// `CONFIGURE`（S→C）：桌面建议的 surface 尺寸（如用户拖动边框缩放窗口）。
///
/// 客户端可按自身网格对齐后通过 [`SetBuffer`] 提交实际尺寸；尺寸不变时可忽略。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Configure {
    /// 目标 surface id。
    pub surface_id: u32,
    /// 建议宽度（像素）。
    pub width: u32,
    /// 建议高度（像素）。
    pub height: u32,
}

impl Configure {
    /// 编码为完整帧写入 `buf`，返回帧总长度；`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        let total = encode_frame(buf, CONFIGURE, 12)?;
        write_u32(buf, 8, self.surface_id)?;
        write_u32(buf, 12, self.width)?;
        write_u32(buf, 16, self.height)?;
        Some(total)
    }

    /// 从 payload 解析；不足 12 字节返回 `None`，多余尾部字节被忽略。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 12 {
            return None;
        }
        Some(Self {
            surface_id: read_u32(payload, 0)?,
            width: read_u32(payload, 4)?,
            height: read_u32(payload, 8)?,
        })
    }
}

/// `INPUT_SYNC_RESET`（S→C）：桌面在 evdev `SYN_DROPPED` 后发送，要求客户端
/// 清空本地维护的 modifier / 按键状态（此后事件流可能与客户端状态不同步）。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputSyncReset {
    /// 目标 surface id。
    pub surface_id: u32,
}

impl InputSyncReset {
    /// 编码为完整帧写入 `buf`，返回帧总长度；`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        let total = encode_frame(buf, INPUT_SYNC_RESET, 4)?;
        write_u32(buf, 8, self.surface_id)?;
        Some(total)
    }

    /// 从 payload 解析；不足 4 字节返回 `None`，多余尾部字节被忽略。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        Some(Self {
            surface_id: read_u32(payload, 0)?,
        })
    }
}

const _: () = assert!(size_of::<Hello>() == 4);
const _: () = assert!(size_of::<Welcome>() == 4);
const _: () = assert!(size_of::<CreateSurface>() == 20);
const _: () = assert!(size_of::<SurfaceCreated>() == 8);
const _: () = assert!(size_of::<Commit>() == 8);
const _: () = assert!(size_of::<InputKey>() == 12);
const _: () = assert!(size_of::<InputPointer>() == 20);
const _: () = assert!(size_of::<Focus>() == 8);
const _: () = assert!(size_of::<CloseRequest>() == 4);
const _: () = assert!(size_of::<SetTitle>() == 8);
const _: () = assert!(size_of::<DestroySurface>() == 4);
const _: () = assert!(size_of::<Configure>() == 12);
const _: () = assert!(size_of::<InputSyncReset>() == 4);
const _: () = assert!(size_of::<SetBuffer>() == 16);

/// `SET_BUFFER`（C→S）：替换 surface 的 backing buffer（语义见 [`crate::SET_BUFFER`]）。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SetBuffer {
    /// 目标 surface id。
    pub surface_id: u32,
    /// 新建 dumb buffer 的 GEM handle；提及时所有权转移给桌面。
    pub gem_handle: u32,
    /// 新内容宽度（像素）。
    pub width: u32,
    /// 新内容高度（像素）。
    pub height: u32,
}

impl SetBuffer {
    /// 编码为完整帧写入 `buf`，返回帧总长度；`buf` 不足或超 [`crate::MAX_MESSAGE`] 时返回 `None`。
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        let total = encode_frame(buf, SET_BUFFER, 16)?;
        write_u32(buf, 8, self.surface_id)?;
        write_u32(buf, 12, self.gem_handle)?;
        write_u32(buf, 16, self.width)?;
        write_u32(buf, 20, self.height)?;
        Some(total)
    }

    /// 从 payload 解析；不足 16 字节返回 `None`，多余尾部字节被忽略。
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 16 {
            return None;
        }
        Some(Self {
            surface_id: read_u32(payload, 0)?,
            gem_handle: read_u32(payload, 4)?,
            width: read_u32(payload, 8)?,
            height: read_u32(payload, 12)?,
        })
    }
}
