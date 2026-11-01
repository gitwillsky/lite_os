//! 启动画面进度：使用者在启动阶段推进，驱动宿主在启动帧缓冲上绘制。

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::Channel;
use native_runtime::native_abi::{self, Status};

use crate::directory::{self, Announcement, Class};
use crate::{Reader, Writer, call, serve, take_handle};

const ADVANCE: u32 = 1;

/// 启动阶段（进度条位置由驱动宿主按阶段映射）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Milestone {
    /// 命名空间与 IPC/socket 全局状态就绪。
    Namespaces = 1,
    /// 任务表与等待协议就绪。
    Scheduler = 2,
    /// 块设备与 console 已发布。
    Devices = 3,
    /// 音频已发现；之后显示交给 DRM 与 compositor。
    Audio = 4,
}

impl Milestone {
    fn decode(value: u32) -> Result<Self, Status> {
        Ok(match value {
            1 => Self::Namespaces,
            2 => Self::Scheduler,
            3 => Self::Devices,
            4 => Self::Audio,
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        })
    }
}

/// 驱动宿主：发布进度通道，`advance` 在收到每个阶段时调用。
///
/// # Errors
///
/// channel、线程创建或目录写入失败。
pub fn publish(
    directory: &Channel,
    advance: Arc<dyn Fn(Milestone) + Send + Sync>,
) -> Result<(), Status> {
    let (client, server) = Channel::create()?;
    serve(
        server,
        1,
        b"boot-progress",
        Arc::try_new(
            move |ordinal: u32, request: &mut Reader<'_>, _, _: &mut Writer| {
                if ordinal != ADVANCE {
                    return Err(native_abi::ERR_NOT_SUPPORTED);
                }
                advance(Milestone::decode(request.u32()?)?);
                Ok(Vec::new())
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?;
    directory::announce(
        directory,
        Class::Boot,
        &directory::description(),
        alloc::vec![client.into_handle()],
    )
}

/// 使用者端的进度通道。
pub struct BootProgress {
    channel: Channel,
}

/// 连接目录中的进度通道。
///
/// # Errors
///
/// 句柄缺失。
pub fn connect(mut announcement: Announcement) -> Result<BootProgress, Status> {
    Ok(BootProgress {
        channel: Channel::from_handle(take_handle(&mut announcement.handles, 0)?),
    })
}

impl BootProgress {
    /// 推进到 `milestone`；驱动宿主画面已停用时同样成功。
    pub fn advance(&self, milestone: Milestone) {
        let mut request = Writer::request(ADVANCE);
        request.u32(milestone as u32);
        let _ = call(&self.channel, &request, Vec::new());
    }
}
