use super::{Result, Widget};
use classic_ui_abi as abi;
use std::os::fd::{AsRawFd, BorrowedFd};

/// 供 Canvas 视频子表面采样的 XRGB8888 共享内存。stride 以字节计。
pub struct VideoBuffer<'a> {
    pub fd: BorrowedFd<'a>,
    pub slot: u32,
    pub generation: u64,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
}

impl Widget {
    /// 提交视频缓冲。返回 true 时生产者必须等 VideoReleased 后再写入；
    /// 返回 false 时视图暂时不可提交，调用方继续独占该缓冲。None 隐藏视频。
    ///
    /// FD 或子表面无效时返回错误。关闭窗口释放客户端持有的所有槽。
    pub fn present_video(&self, buffer: Option<VideoBuffer<'_>>) -> Result<bool> {
        let input = buffer.map(|buffer| abi::VideoFrame {
            fd: buffer.fd.as_raw_fd(),
            slot: buffer.slot,
            generation: buffer.generation,
            width: buffer.width,
            height: buffer.height,
            stride: buffer.stride,
        });
        let mut pending = false;
        self.call(|c, w, id| {
            // SAFETY: 只在同步调用期间借用 ABI 描述记录和它的有效 FD。
            let status = unsafe {
                (c.api.canvas_video)(
                    c.context.as_ptr(),
                    w,
                    id,
                    input.as_ref().map_or(std::ptr::null(), std::ptr::from_ref),
                )
            };
            pending = status == abi::PENDING;
            if pending { abi::OK } else { status }
        })?;
        Ok(!pending)
    }
}
