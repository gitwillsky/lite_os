//! 回退显示客户端（Fuchsia virtcon）：主显示会话之间由驱动宿主自己显示回退画面。

use alloc::sync::Arc;

use device_model::{DeviceBacking, DisplayError, DisplayMode, DisplayUpdate, GraphicsDevice};
use native_runtime::Event;
use native_runtime::native_abi::{self, signals};
use native_runtime::sync::Mutex;

/// 回退画面（Fuchsia virtcon 的角色）：没有显示会话时由驱动宿主自己占有 scanout。
pub trait FallbackScene: Send + Sync {
    /// 把回退画面画进 `mode` 大小的线性 XRGB8888 缓冲；失败时返回 false，显示保持黑屏。
    fn paint(&self, mode: DisplayMode, backing: &DeviceBacking) -> bool;
}

/// 回退画面 scanout 的 framebuffer identity；会话的 identity 由使用者从 1 单调分配，不会到达该值。
const VIRTCON_IDENTITY: u64 = u64::MAX;
/// 回退画面每个事务等待设备完成的上限；超时只放弃回退画面，不阻止会话交接。
const VIRTCON_SETTLE_NS: i64 = 1_000_000_000;

/// 宿主侧的回退显示客户端。
///
/// 1. 会话结束（[`DisplaySession`] 回收其对象之后）时 [`Virtcon::show`] 画出回退画面并提交 scanout；
/// 2. 新会话交出之前 [`Virtcon::dismiss`] 等回退事务完成，再 disable scanout 并释放缓冲，
///    新使用者看到的设备与开机后首次打开时一样没有遗留对象；
/// 3. 二者都在独占会话的边界上同步完成：没有会话时活动 Event 只有宿主自己等待，不与使用者争抢。
pub(super) struct Virtcon {
    pub(super) device: Arc<dyn GraphicsDevice>,
    pub(super) event: Arc<Event>,
    pub(super) scene: Arc<dyn FallbackScene>,
    // OWNER: 回退画面当前占用的 scanout 缓冲；None 表示显示归会话或开机画面。缺失时交接无法判断
    // 是否需要撤下回退 scanout，新会话会继承一个它不知道的 resident resource。
    pub(super) shown: Mutex<Option<Arc<DeviceBacking>>>,
}

impl Virtcon {
    pub(super) fn show(&self) {
        let mut shown = self.shown.lock();
        if shown.is_some() {
            return;
        }
        let mode = self.device.mode();
        let bytes = mode.pitch as usize * mode.height as usize;
        let Some(backing) = DeviceBacking::try_allocate(bytes.div_ceil(native_abi::PAGE_SIZE))
            .and_then(|backing| Arc::try_new(backing).ok())
        else {
            native_runtime::warn!("virtcon: framebuffer allocation failed");
            return;
        };
        if !self.scene.paint(mode, &backing) {
            return;
        }
        let submitted = self.transact(|| {
            self.device
                .submit_scanout(VIRTCON_IDENTITY, mode, backing.clone())
                .map(Some)
        });
        match submitted {
            Ok(()) => *shown = Some(backing),
            Err(error) => {
                native_runtime::warn!("virtcon: fallback scanout failed: {error:?}");
                // 失败的提交可能已让 resource resident；按交接路径回收，不留给下一个会话。
                let _ = self.transact(|| self.device.release_buffer(VIRTCON_IDENTITY));
            }
        }
    }

    pub(super) fn dismiss(&self) {
        if self.shown.lock().take().is_none() {
            return;
        }
        let result = self
            .transact(|| self.device.disable_scanout().map(Some))
            .and_then(|()| self.transact(|| self.device.release_buffer(VIRTCON_IDENTITY)));
        if let Err(error) = result {
            native_runtime::warn!("virtcon: handover failed: {error:?}");
        }
    }

    /// 提交一个 operation（已有事务时先推进到可提交），并推进到它的完成。
    fn transact(
        &self,
        mut submit: impl FnMut() -> Result<Option<u64>, DisplayError>,
    ) -> Result<(), DisplayError> {
        let deadline = native_runtime::system::monotonic().saturating_add(VIRTCON_SETTLE_NS);
        let fence = loop {
            match submit() {
                Err(DisplayError::WouldBlock) => {
                    self.pump(None)?;
                    self.wait(deadline)?;
                }
                result => break result?,
            }
        };
        let Some(fence) = fence else {
            return Ok(());
        };
        loop {
            if self.pump(Some(fence))? {
                return Ok(());
            }
            self.wait(deadline)?;
        }
    }

    /// 先清除活动信号再拉取全部更新，之后到达的完成会重新置位；返回是否看到 `fence` 完成。
    fn pump(&self, fence: Option<u64>) -> Result<bool, DisplayError> {
        let _ = self.event.signal(signals::USER_0, 0);
        let mut completed = false;
        while let Some(update) = self.device.poll_update()? {
            completed |=
                fence.is_some_and(|fence| update == DisplayUpdate::OperationCompleted(fence));
        }
        Ok(completed)
    }

    fn wait(&self, deadline: i64) -> Result<(), DisplayError> {
        self.event
            .wait_one(signals::USER_0, deadline)
            .map(drop)
            .map_err(|_| DisplayError::Device)
    }
}
