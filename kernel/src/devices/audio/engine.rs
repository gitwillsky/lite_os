//! 多子流混音引擎：硬件流的唯一 owner。
//!
//! 引擎独占那一个物理 playback 流，并在它之上提供 [`SUBSTREAMS`] 个互相独立的软件子流。应用只写自己子流的
//! 环形缓冲并推进应用指针；引擎在每次"应用提交"与"硬件 period 完成"之后把所有运行中子流的下一个 period
//! 混成一个硬件 period 提交，最多预送 [`LEAD_PERIODS`] 段：覆盖宿主批量消费，同时保留一段应用数据供完成后续送。
//!
//! 引擎是纯状态机：设备通过 [`PcmOutput`]、环形缓冲通过 [`Ring`] 传入，不持有任何锁，也不认识 OS。
//! 调用方负责串行化（一个 `Mutex<Engine>`）。硬件完成事件只会被记成计数后经 [`Events`] 交给 `tick`，所以设备
//! 回调里不需要、也不允许再取引擎的锁。

use super::control::Element;
use super::mixer::{Accumulator, VOLUME_MAX, volume_gain};
use super::state::{PcmState, PcmStateOwner};
use crate::devices::model::{
    MAX_PCM_PERIOD_BYTES, PCM_PERIOD_BYTES, PCM_PERIODS, PcmConfiguration, PcmOutput,
    PcmOutputError,
};

/// 同时可打开的 playback 子流数量，即 `snd_pcm_info.subdevices_count`。
pub(super) const SUBSTREAMS: usize = 8;
/// 保留一段已提交的应用数据：设备可一次归还整批 TX，全送进设备会在写进程获调度前把应用判成 Xrun。
/// 其余容量全部用于预送；额外缩短深度会使宿主消费与补充之间缺少余量，产生断音。
const LEAD_PERIODS: usize = PCM_PERIODS - 1;

const _: () = assert!(
    SUBSTREAMS <= u8::BITS as usize,
    "running/ready masks are one byte"
);
const _: () = assert!(PCM_PERIODS > 1);
/// 子流环形缓冲的读取接口；`offset` 是缓冲内字节偏移。
pub(super) trait Ring {
    /// 把 `output.len()` 个字节读入 `output`，范围越界时失败。
    fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), ()>;
}

/// 自上次 `tick` 以来设备回调累计的事件。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Events {
    /// 完成的硬件 period 数。
    pub(super) completed: usize,
    /// 设备已断开，再也不会完成任何 period。
    pub(super) disconnected: bool,
}

/// 物理流的生命周期，与 VirtIO Sound 的 Prepared/Running/Stopped 对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hardware {
    Unconfigured,
    Transition,
    /// 已 prepare、尚未 start：可以提交 period，但设备不消费。
    Stopped,
    Running,
    /// 设备失败或断开，永久不可用。
    Failed,
}

/// 主音量与静音开关，左右声道各一份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Master {
    /// 0..=[`VOLUME_MAX`]。
    pub(super) volume: [u32; 2],
    /// `false` 表示该声道静音。
    pub(super) enabled: [bool; 2],
}

/// 每个在途物理 period 记录各子流实际贡献的帧数；短尾帧不能按完整 period 推进 hw_ptr。
#[derive(Debug, Default)]
struct PeriodQueue {
    frames: [[u16; SUBSTREAMS]; PCM_PERIODS],
    head: usize,
    len: usize,
}

impl PeriodQueue {
    fn push(&mut self, frames: [u16; SUBSTREAMS]) -> bool {
        if self.len == PCM_PERIODS {
            return false;
        }
        self.frames[(self.head + self.len) % PCM_PERIODS] = frames;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<[u16; SUBSTREAMS]> {
        if self.len == 0 {
            return None;
        }
        let frames = self.frames[self.head];
        self.head = (self.head + 1) % PCM_PERIODS;
        self.len -= 1;
        Some(frames)
    }

    /// 摘掉一个子流在所有在途 period 里的贡献：这些 period 仍会播放，但完成时不再给它记 hw_ptr。
    /// 缺少这一步，子流被 drop/重新 prepare 后会被旧 period 的完成事件错误地推进指针。
    fn forget(&mut self, substream: usize) {
        for frames in &mut self.frames {
            frames[substream] = 0;
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

struct Slot<R> {
    state: PcmStateOwner,
    /// 已混入硬件队列的帧数；与 `state.application_frames` 同一计数域，且不超过应用指针。
    mixed: u64,
    ring: Option<R>,
}

impl<R> Slot<R> {
    const fn vacant() -> Self {
        Self {
            state: PcmStateOwner::new(),
            mixed: 0,
            ring: None,
        }
    }

    fn running(&self) -> bool {
        self.ring.is_some() && self.state.state == PcmState::Running
    }

    /// 应用已写入、尚未混进硬件的帧数。
    fn pending(&self) -> u64 {
        self.state.application_frames.wrapping_sub(self.mixed)
    }
}

/// 一个硬件流加 [`SUBSTREAMS`] 个子流的混音状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PcmEndpoint {
    Shared,
    Direct,
}

impl PcmEndpoint {
    pub(super) const fn name(self) -> &'static [u8] {
        match self {
            Self::Shared => b"LiteOS Playback",
            Self::Direct => b"LiteOS Native (Exclusive)",
        }
    }
    pub(super) const fn device(self) -> u32 {
        match self {
            Self::Shared => 0,
            Self::Direct => 1,
        }
    }
}

pub(super) struct Engine<R> {
    endpoint: PcmEndpoint,
    configuration: Option<PcmConfiguration>,
    // OWNER: one bounded output scratch belongs to the physical stream, avoiding hot-path allocation.
    period: [u8; MAX_PCM_PERIOD_BYTES],
    slots: [Slot<R>; SUBSTREAMS],
    queue: PeriodQueue,
    hardware: Hardware,
    master: Master,
    substream_volume: [[u32; 2]; SUBSTREAMS],
}

fn each_bit(mask: u8) -> impl Iterator<Item = usize> {
    (0..SUBSTREAMS).filter(move |index| mask & (1 << index) != 0)
}

impl<R: Ring> Engine<R> {
    /// 新引擎：物理流已 prepare 好（由 `audio::init` 在构造之前完成），所有子流空闲，音量满、不静音。
    pub(super) fn new(configuration: Option<PcmConfiguration>) -> Self {
        Self {
            endpoint: PcmEndpoint::Shared,
            configuration,
            period: [0; MAX_PCM_PERIOD_BYTES],
            slots: core::array::from_fn(|_| Slot::vacant()),
            queue: PeriodQueue::default(),
            substream_volume: [[VOLUME_MAX; 2]; SUBSTREAMS],
            hardware: if configuration.is_some() {
                Hardware::Stopped
            } else {
                Hardware::Unconfigured
            },
            master: Master {
                volume: [VOLUME_MAX; 2],
                enabled: [true; 2],
            },
        }
    }

    /// 按 Linux control 偏好占用子流；-1 选择首个空闲槽，其余值只接受精确编号。
    ///
    /// # Returns
    ///
    /// 子流编号；全部占满时把 `ring` 原样退回，调用方在锁外释放它。
    pub(super) fn allocate(
        &mut self,
        ring: R,
        preferred: i32,
        endpoint: PcmEndpoint,
    ) -> Result<usize, R> {
        let occupied = self.slots.iter().any(|slot| slot.ring.is_some());
        if (endpoint == PcmEndpoint::Direct && (occupied || preferred > 0))
            || (occupied && self.endpoint != endpoint)
        {
            return Err(ring);
        }
        let Some(index) = self.slots.iter().enumerate().position(|(index, slot)| {
            slot.ring.is_none() && (preferred == -1 || preferred == index as i32)
        }) else {
            return Err(ring);
        };
        self.endpoint = endpoint;
        let slot = &mut self.slots[index];
        *slot = Slot::vacant();
        slot.ring = Some(ring);
        if self.hardware == Hardware::Failed {
            slot.state.disconnect();
        }
        Ok(index)
    }

    /// 释放子流，返回它的环形缓冲以便调用方在锁外销毁。
    pub(super) fn release(&mut self, index: usize) -> Option<R> {
        self.queue.forget(index);
        let ring = self.slots[index].ring.take();
        self.slots[index] = Slot::vacant();
        ring
    }

    /// 尚未被占用的子流数量，即 `subdevices_avail`。
    pub(super) fn available(&self) -> usize {
        self.slots.iter().filter(|slot| slot.ring.is_none()).count()
    }

    pub(super) fn state(&self, index: usize) -> &PcmStateOwner {
        &self.slots[index].state
    }

    /// 读取唯一混音状态中的控制值，不复制 Master 或子流音量状态。
    pub(super) fn control_values(&self, element: Element) -> [u32; 2] {
        match element {
            Element::MasterVolume => self.master.volume,
            Element::MasterSwitch => self.master.enabled.map(u32::from),
            Element::PcmVolume(index) => self.substream_volume[index],
        }
    }

    /// 应用 codec 已校验的值，返回是否发生真实变化，供 control 事件通知使用。
    pub(super) fn set_control(&mut self, element: Element, values: [u32; 2]) -> bool {
        if self.control_values(element) == values {
            return false;
        }
        match element {
            Element::MasterVolume => self.master.volume = values,
            Element::MasterSwitch => self.master.enabled = values.map(|value| value != 0),
            Element::PcmVolume(index) => self.substream_volume[index] = values,
        }
        true
    }

    /// 清掉子流在引擎里的全部痕迹：混音位置与在途 period 的归属。回到 Setup 或重新 prepare 之前必须调用，
    /// 否则旧 period 的完成事件会推进新一轮播放的 hw_ptr。
    fn detach(&mut self, index: usize) {
        self.queue.forget(index);
        self.slots[index].mixed = 0;
    }

    /// hw_params 生效：Open → Setup。
    pub(super) fn configure(
        &mut self,
        index: usize,
        buffer_frames: u64,
        configuration: PcmConfiguration,
    ) -> Result<(), ()> {
        self.slots[index]
            .state
            .configure(buffer_frames, configuration)
    }

    pub(super) fn configuration(&self) -> Option<PcmConfiguration> {
        self.configuration
    }

    /// 冻结物理提交；调用者持 task 级配置锁，在释放 Engine 锁后完成可睡眠的设备控制事务。
    pub(super) fn begin_transition(&mut self) -> Result<bool, PcmOutputError> {
        if matches!(self.hardware, Hardware::Failed | Hardware::Transition) {
            return Err(PcmOutputError::Device);
        }
        let running = self.hardware == Hardware::Running;
        self.hardware = Hardware::Transition;
        Ok(running)
    }

    pub(super) fn finish_transition(
        &mut self,
        configuration: Option<PcmConfiguration>,
    ) -> Result<(), PcmOutputError> {
        if self.hardware != Hardware::Transition {
            return Err(PcmOutputError::Device);
        }
        self.queue.clear();
        self.configuration = configuration;
        self.hardware = if configuration.is_some() {
            Hardware::Stopped
        } else {
            Hardware::Unconfigured
        };
        Ok(())
    }

    pub(super) fn running(&self) -> bool {
        self.hardware == Hardware::Running
    }

    pub(super) fn endpoint_available(&self, endpoint: PcmEndpoint) -> usize {
        if self.slots.iter().any(|slot| slot.ring.is_some()) && self.endpoint != endpoint {
            0
        } else if endpoint == PcmEndpoint::Direct {
            usize::from(self.available() == SUBSTREAMS)
        } else {
            self.available()
        }
    }

    /// 设置唤醒、启动、停止阈值与 boundary。
    pub(super) fn set_software(
        &mut self,
        index: usize,
        available_min: u64,
        start_threshold: u64,
        stop_threshold: u64,
        boundary: u64,
    ) -> Result<(), ()> {
        self.slots[index].state.set_software(
            available_min,
            start_threshold,
            stop_threshold,
            boundary,
        )
    }

    /// Setup、Prepared 或 Xrun → Prepared，指针清零并撤销旧 period 归属。
    pub(super) fn prepare(&mut self, index: usize) -> Result<(), ()> {
        self.slots[index].state.prepare()?;
        self.detach(index);
        Ok(())
    }

    /// Setup → Open。
    pub(super) fn free_hardware(&mut self, index: usize) -> Result<(), ()> {
        self.slots[index].state.free_hardware()
    }

    /// Prepared → Running；真正的数据在随后的 `tick` 里混入。
    pub(super) fn start(&mut self, index: usize) -> Result<(), ()> {
        self.slots[index].state.start()
    }

    /// 立即停止并回到 Setup；已混入硬件的 period 仍会播放完，但不再给本子流记账。
    pub(super) fn drop_stream(&mut self, index: usize) -> Result<(), ()> {
        self.slots[index].state.drop_stream()?;
        self.detach(index);
        Ok(())
    }

    /// 应用提交了 `frames` 帧（数据已经在环形缓冲里）。
    pub(super) fn application_frames(&mut self, index: usize, frames: u64) -> Result<(), ()> {
        self.slots[index].state.submit_frames(frames)
    }

    /// 设备回调累积的事件交给引擎：记账完成的 period，补填队列，按需启停物理流，并检出欠载的子流。
    ///
    /// # Returns
    ///
    /// 任何子流的状态或指针发生变化时为 `true`，调用方据此唤醒等待者。
    pub(super) fn tick(&mut self, events: Events, output: &dyn PcmOutput) -> bool {
        if self.hardware == Hardware::Failed {
            return false;
        }
        if events.disconnected {
            self.fail();
            return true;
        }
        if self.hardware == Hardware::Unconfigured {
            return false;
        }
        let mut changed = false;
        // 1. 完成的 period 只给当时贡献过数据的子流记 hw_ptr。
        for _ in 0..events.completed {
            let Some(frames) = self.queue.pop() else {
                break;
            };
            for (index, frames) in frames.into_iter().enumerate() {
                if frames != 0 {
                    self.slots[index].state.complete_period(u64::from(frames));
                }
            }
            changed = true;
        }
        if self.hardware == Hardware::Transition {
            return changed;
        }
        // 2. 先补填：设备可一次归还多个 TX，暂时没有在途 period 不代表应用断粮。
        match self.fill(output) {
            Ok(filled) => changed |= filled,
            Err(()) => {
                self.fail();
                return true;
            }
        }
        // 3. 补填后仍为空才停止并 prepare；先停止会打断设备已接收的连续 PCM，造成反复停启和卡顿。
        if self.endpoint == PcmEndpoint::Shared
            && self.hardware == Hardware::Running
            && self.queue.len == 0
        {
            if Self::rearm(output).is_err() {
                self.fail();
                return true;
            }
            self.hardware = Hardware::Stopped;
        }
        // 4. 有在途数据而设备没在跑：启动。
        if self.hardware == Hardware::Stopped && self.queue.len > 0 {
            if output.start().is_err() {
                self.fail();
                return true;
            }
            self.hardware = Hardware::Running;
        }
        // 5. 运行中的子流既没有可混的数据、也没有在途数据：它已经跑空。
        for slot in &mut self.slots {
            if slot.running() && slot.pending() == 0 && slot.mixed == slot.state.hardware_frames {
                slot.state.xrun();
                changed = true;
            }
        }
        changed
    }

    fn rearm(output: &dyn PcmOutput) -> Result<(), PcmOutputError> {
        output.stop()?;
        output.prepare()
    }

    /// 按 [`LEAD_PERIODS`] 补填硬件队列，留出一段应用数据跨越整批完成。
    ///
    /// # Returns
    ///
    /// 提交了 period 或有子流因此进入 Xrun 时为 `true`；设备失败时返回 `Err`。
    fn fill(&mut self, output: &dyn PcmOutput) -> Result<bool, ()> {
        let Some(configuration) = self.configuration else {
            return Ok(false);
        };
        let frame_bytes = configuration.frame_bytes();
        let period_frames = configuration.period_frames;
        let period_bytes = configuration.period_bytes();
        let mut changed = false;
        while self.queue.len < LEAD_PERIODS && output.writable() {
            let mut running = 0u8;
            let mut ready = 0u8;
            for (index, slot) in self.slots.iter().enumerate() {
                if slot.running() {
                    running |= 1 << index;
                    // 整周期照常预送；短批次等本流在途数据耗尽才输出，避免过早插入静音间隙。
                    if slot.pending() >= period_frames as u64
                        || slot.pending() != 0 && slot.mixed == slot.state.hardware_frames
                    {
                        ready |= 1 << index;
                    }
                }
            }
            if ready == 0 {
                break;
            }
            // 缺少未来数据的流若仍有在途 period，交付期限尚未到；先等它补充，不能提前混入缺口或报 Xrun。
            if each_bit(running & !ready)
                .any(|index| self.slots[index].mixed != self.slots[index].state.hardware_frames)
            {
                break;
            }
            // 这个 period 不含这些子流的数据：它们的时间线出现了缺口，按 ALSA 语义就是 underrun。
            for index in each_bit(running & !ready) {
                self.slots[index].state.xrun();
                changed = true;
            }
            let mut accumulator = Accumulator::new();
            let mut source = [0u8; PCM_PERIOD_BYTES];
            let mut mixed = 0u8;
            let mut contributions = [0u16; SUBSTREAMS];
            for index in each_bit(ready) {
                let slot = &self.slots[index];
                let offset = (slot.mixed % slot.state.buffer_frames) as usize * frame_bytes;
                let frames = slot.pending().min(period_frames as u64) as usize;
                let bytes = frames * frame_bytes;
                let first = bytes.min(slot.state.buffer_frames as usize * frame_bytes - offset);
                let read = slot.ring.as_ref().map(|ring| {
                    let period = if self.endpoint == PcmEndpoint::Direct {
                        &mut self.period[..period_bytes]
                    } else {
                        &mut source[..]
                    };
                    ring.read(offset, &mut period[..first])?;
                    if first < bytes {
                        ring.read(0, &mut period[first..bytes])?;
                    }
                    Ok::<(), ()>(())
                });
                if read == Some(Ok(())) {
                    if self.endpoint == PcmEndpoint::Direct {
                        self.period[bytes..period_bytes].fill(0);
                    } else {
                        source[bytes..].fill(0);
                        accumulator.add(&source, self.substream_volume[index].map(volume_gain));
                    }
                    mixed |= 1 << index;
                    contributions[index] = frames as u16;
                } else {
                    // 环形缓冲读取不应失败；失败的子流退出这个 period，而不是让混音带着垃圾数据出声。
                    self.slots[index].state.xrun();
                    changed = true;
                }
            }
            if mixed == 0 {
                continue;
            }
            if self.endpoint == PcmEndpoint::Shared {
                let gain = self.output_gain();
                accumulator.finish(
                    gain,
                    (&mut self.period[..PCM_PERIOD_BYTES])
                        .try_into()
                        .map_err(|_| ())?,
                );
            }
            match output.submit_period(&self.period[..period_bytes]) {
                Ok(()) => {}
                Err(PcmOutputError::WouldBlock) => break,
                Err(_) => return Err(()),
            }
            let queued = self.queue.push(contributions);
            debug_assert!(queued, "queue depth is bounded by LEAD_PERIODS");
            for index in each_bit(mixed) {
                self.slots[index].mixed = self.slots[index]
                    .mixed
                    .wrapping_add(u64::from(contributions[index]));
            }
            changed = true;
        }
        Ok(changed)
    }

    fn output_gain(&self) -> [f32; 2] {
        let channel = |index: usize| {
            if self.master.enabled[index] {
                volume_gain(self.master.volume[index])
            } else {
                0.0
            }
        };
        [channel(0), channel(1)]
    }

    /// 设备永久失败：所有子流断开，在途记账作废。
    pub(super) fn fail(&mut self) {
        self.hardware = Hardware::Failed;
        self.queue.clear();
        for slot in &mut self.slots {
            if slot.ring.is_some() {
                slot.state.disconnect();
            }
        }
    }
}
