//! 启动画面进度：使用者在启动阶段推进，驱动宿主在启动帧缓冲上绘制。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::Channel;
use native_runtime::native_abi::{self, Status};

use crate::directory::{self, Class, Directory, OpenedDevice, Session, SessionState};
use crate::{Reader, Writer, call, take_handle};

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

/// 驱动宿主：登记进度通道（可共享：各阶段的 owner 各自推进），`advance` 在收到每个阶段时调用。
///
/// # Errors
///
/// 分配失败。
pub fn publish(
    directory: &Directory,
    advance: Arc<dyn Fn(Milestone) + Send + Sync>,
) -> Result<(), Status> {
    directory.add(
        Class::Boot,
        &directory::description(),
        false,
        Box::new(move |end| {
            let session = SessionState::new(advance.clone(), end);
            let (client, server) = Channel::create()?;
            let server = directory::serve_session(
                server,
                1,
                b"boot-progress",
                Arc::try_new(
                    move |ordinal: u32, request: &mut Reader<'_>, _, _: &mut Writer| {
                        if ordinal != ADVANCE {
                            return Err(native_abi::ERR_NOT_SUPPORTED);
                        }
                        (session.get())(Milestone::decode(request.u32()?)?);
                        Ok(Vec::new())
                    },
                )
                .map_err(|_| native_abi::ERR_NO_MEMORY)?,
            )?;
            Ok(Session {
                handles: alloc::vec![client.into_handle()],
                server,
            })
        }),
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
pub fn connect(mut announcement: OpenedDevice) -> Result<BootProgress, Status> {
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
