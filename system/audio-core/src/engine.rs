//! 混音引擎：一个物理输出流的唯一 owner，在其上提供 [`STREAMS`] 个互相独立的渲染流。
//!
//! 使用者只写自己流的环形缓冲并发布已写入帧数；引擎在每次“使用者写入”与“硬件 period 完成”之后，把所有
//! 播放中流的下一个 period 混成一个硬件 period 提交，最多预送 [`LEAD_PERIODS`] 段：覆盖宿主批量消费，
//! 同时保留一段使用者数据供完成后续送。
//!
//! 引擎是纯状态机：设备经 [`PcmOutput`] 传入，不持有任何锁。调用方串行化（每个输出一个 `Mutex<Engine>`）。
//! 硬件完成事件只记成计数后经 [`Events`] 交给 `tick`，设备回调里不需要也不允许取引擎的锁。

use device_model::{
    MAX_PCM_PERIOD_BYTES, PCM_PERIOD_BYTES, PCM_PERIODS, PcmConfiguration, PcmOutput,
    PcmOutputError,
};

use super::mix::{Accumulator, VOLUME_MAX, volume_gain};

/// 一个输出上同时存在的渲染流数量（ALSA 共享端点的子流数）。
pub(crate) const STREAMS: usize = 8;
/// 保留一段已提交的使用者数据：设备可一次归还整批 TX，全送进设备会在写者获调度前判成欠载。
/// 其余容量全部用于预送；额外缩短深度会使宿主消费与补充之间缺少余量，产生断音。
const LEAD_PERIODS: usize = PCM_PERIODS - 1;

const _: () = assert!(
    STREAMS <= u8::BITS as usize,
    "running/ready masks are one byte"
);
const _: () = assert!(PCM_PERIODS > 1);

/// 自上次 `tick` 以来设备回调累计的事件。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Events {
    /// 完成的硬件 period 数。
    pub(crate) completed: usize,
    /// 设备已断开，再也不会完成任何 period。
    pub(crate) disconnected: bool,
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

/// 每个在途物理 period 记录各流实际贡献的帧数；短尾帧不能按完整 period 推进播放位置。
#[derive(Debug, Default)]
struct PeriodQueue {
    frames: [[u16; STREAMS]; PCM_PERIODS],
    head: usize,
    len: usize,
}

impl PeriodQueue {
    fn push(&mut self, frames: [u16; STREAMS]) -> bool {
        if self.len == PCM_PERIODS {
            return false;
        }
        self.frames[(self.head + self.len) % PCM_PERIODS] = frames;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<[u16; STREAMS]> {
        if self.len == 0 {
            return None;
        }
        let frames = self.frames[self.head];
        self.head = (self.head + 1) % PCM_PERIODS;
        self.len -= 1;
        Some(frames)
    }

    /// 摘掉一个流在所有在途 period 里的贡献：这些 period 仍会播放，但完成时不再给它记位置。
    /// 缺少这一步，流被停止或释放后会被旧 period 的完成事件错误地推进位置。
    fn forget(&mut self, stream: usize) {
        for frames in &mut self.frames {
            frames[stream] = 0;
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// 渲染流的端点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endpoint {
    /// 与其他共享流混音。
    Shared,
    /// 独占输出，以协商格式直通设备。
    Exclusive,
}

/// 输出的主音量与静音开关，左右声道各一份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Master {
    /// 0..=[`VOLUME_MAX`]。
    pub(crate) volume: [u32; 2],
    /// `false` 表示该声道静音。
    pub(crate) enabled: [bool; 2],
}

/// 发布给使用者的流状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamState {
    Idle,
    Playing,
    /// 运行中的流没有可混的数据：时间线出现缺口（ALSA underrun），等待使用者停止后重新开始。
    Underrun,
    /// 输出设备失败或断开，流永久不可用。
    Disconnected,
}

/// 渲染流与使用者之间的数据面：环形缓冲、已写入帧数与位置发布。
pub(crate) trait StreamIo {
    /// 从环形缓冲字节偏移 `offset` 读满 `output`；越界时失败。
    fn read(&self, offset: u64, output: &mut [u8]) -> Result<(), ()>;
    /// 使用者已写入的帧数（自上次停止起累计）。
    fn written(&self) -> u64;
    /// 停止时清零使用者的已写入帧数。
    fn reset_written(&self);
    /// 发布播放位置与状态并唤醒使用者。
    fn publish(&self, played: u64, state: StreamState);
}

/// 一个渲染流：数据面与混音位置。
pub(crate) struct Stream<S> {
    io: S,
    buffer_frames: u64,
    state: StreamState,
    /// 已混入硬件队列的帧数；与使用者的已写入帧数同一计数域，且不超过它。
    mixed: u64,
    /// 设备已消费的帧数。
    played: u64,
    gain: [u32; 2],
}

impl<S: StreamIo> Stream<S> {
    pub(crate) fn new(io: S, buffer_frames: u64) -> Self {
        Self {
            io,
            buffer_frames,
            state: StreamState::Idle,
            mixed: 0,
            played: 0,
            gain: [VOLUME_MAX; 2],
        }
    }

    /// 数据面（`output` 用它取得原生对象）。
    pub(crate) fn io(&self) -> &S {
        &self.io
    }

    pub(crate) fn io_mut(&mut self) -> &mut S {
        &mut self.io
    }

    fn playing(&self) -> bool {
        self.state == StreamState::Playing
    }

    /// 使用者已写入、尚未混进硬件的帧数（不超过环形缓冲容量）。
    fn pending(&self) -> u64 {
        self.io
            .written()
            .wrapping_sub(self.mixed)
            .min(self.buffer_frames)
    }

    fn underrun(&mut self) {
        self.state = StreamState::Underrun;
    }

    fn publish(&self) {
        self.io.publish(self.played, self.state);
    }
}

pub(crate) struct Engine<S> {
    endpoint: Endpoint,
    configuration: Option<PcmConfiguration>,
    // OWNER: one bounded output scratch belongs to the physical stream, avoiding hot-path allocation.
    period: [u8; MAX_PCM_PERIOD_BYTES],
    streams: [Option<Stream<S>>; STREAMS],
    queue: PeriodQueue,
    hardware: Hardware,
    master: Master,
}

fn each_bit(mask: u8) -> impl Iterator<Item = usize> {
    (0..STREAMS).filter(move |index| mask & (1 << index) != 0)
}

impl<S: StreamIo> Engine<S> {
    /// 新引擎：物理流已按 `configuration` prepare 好，所有流空闲，音量满、不静音。
    pub(crate) fn new(configuration: Option<PcmConfiguration>) -> Self {
        Self {
            endpoint: Endpoint::Shared,
            configuration,
            period: [0; MAX_PCM_PERIOD_BYTES],
            streams: core::array::from_fn(|_| None),
            queue: PeriodQueue::default(),
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

    /// 是否还能在 `endpoint` 上建立流：独占端点要求输出空闲，两种端点不能共存。
    pub(crate) fn admits(&self, endpoint: Endpoint) -> bool {
        let occupied = self.streams.iter().any(Option::is_some);
        if !occupied {
            return true;
        }
        endpoint == Endpoint::Shared
            && self.endpoint == Endpoint::Shared
            && self.streams.iter().any(Option::is_none)
    }

    /// 占用首个空闲槽。
    ///
    /// # Returns
    ///
    /// 流编号；不能接纳时把流原样退回，调用方在锁外释放它。
    pub(crate) fn allocate(
        &mut self,
        endpoint: Endpoint,
        stream: Stream<S>,
    ) -> Result<usize, Stream<S>> {
        if !self.admits(endpoint) {
            return Err(stream);
        }
        let Some(index) = self.streams.iter().position(Option::is_none) else {
            return Err(stream);
        };
        self.endpoint = endpoint;
        let slot = self.streams[index].insert(stream);
        if self.hardware == Hardware::Failed {
            slot.state = StreamState::Disconnected;
            slot.publish();
        }
        Ok(index)
    }

    /// 释放流，返回它以便调用方在锁外销毁。
    pub(crate) fn release(&mut self, index: usize) -> Option<Stream<S>> {
        self.queue.forget(index);
        self.streams[index].take()
    }

    /// 第 `index` 个流。
    pub(crate) fn stream(&self, index: usize) -> Option<&Stream<S>> {
        self.streams[index].as_ref()
    }

    pub(crate) fn stream_mut(&mut self, index: usize) -> Option<&mut Stream<S>> {
        self.streams[index].as_mut()
    }

    /// 当前流的端点。
    pub(crate) fn endpoint(&self) -> Endpoint {
        self.endpoint
    }

    /// 输出上是否还有流。
    pub(crate) fn occupied(&self) -> bool {
        self.streams.iter().any(Option::is_some)
    }

    pub(crate) fn controls(&self) -> Master {
        self.master
    }

    /// 设置主音量与静音，返回是否发生真实变化。
    pub(crate) fn set_controls(&mut self, controls: Master) -> bool {
        let changed = self.master != controls;
        self.master = controls;
        changed
    }

    pub(crate) fn set_gain(&mut self, index: usize, gain: [u32; 2]) -> Result<(), ()> {
        self.streams[index].as_mut().ok_or(())?.gain = gain;
        Ok(())
    }

    /// 开始播放。
    pub(crate) fn start(&mut self, index: usize) -> Result<(), ()> {
        let stream = self.streams[index].as_mut().ok_or(())?;
        if stream.state == StreamState::Disconnected {
            return Err(());
        }
        stream.state = StreamState::Playing;
        stream.publish();
        Ok(())
    }

    /// 停止并清零帧计数；已混入硬件的 period 仍会播放完，但不再给本流记账。
    pub(crate) fn stop(&mut self, index: usize) -> Result<(), ()> {
        self.queue.forget(index);
        let stream = self.streams[index].as_mut().ok_or(())?;
        stream.mixed = 0;
        stream.played = 0;
        if stream.state != StreamState::Disconnected {
            stream.state = StreamState::Idle;
        }
        stream.io.reset_written();
        stream.publish();
        Ok(())
    }

    pub(crate) fn configuration(&self) -> Option<PcmConfiguration> {
        self.configuration
    }

    /// 冻结物理提交；调用者持配置锁，在释放引擎锁后完成可睡眠的设备控制事务。
    pub(crate) fn begin_transition(&mut self) -> Result<bool, PcmOutputError> {
        if matches!(self.hardware, Hardware::Failed | Hardware::Transition) {
            return Err(PcmOutputError::Device);
        }
        let running = self.hardware == Hardware::Running;
        self.hardware = Hardware::Transition;
        Ok(running)
    }

    pub(crate) fn finish_transition(
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

    /// 设备已永久失败。
    pub(crate) fn failed(&self) -> bool {
        self.hardware == Hardware::Failed
    }

    pub(crate) fn running(&self) -> bool {
        self.hardware == Hardware::Running
    }

    /// 设备回调累积的事件交给引擎：记账完成的 period，补填队列，按需启停物理流，并检出欠载的流；
    /// 位置或状态变化的流发布给使用者。
    pub(crate) fn tick(&mut self, events: Events, output: &dyn PcmOutput) {
        if self.hardware == Hardware::Failed {
            return;
        }
        if events.disconnected {
            self.fail();
            return;
        }
        if self.hardware == Hardware::Unconfigured {
            return;
        }
        let mut changed = 0u8;
        // 1. 完成的 period 只给当时贡献过数据的流记位置。
        for _ in 0..events.completed {
            let Some(frames) = self.queue.pop() else {
                break;
            };
            for (index, frames) in frames.into_iter().enumerate() {
                if frames != 0
                    && let Some(stream) = self.streams[index].as_mut()
                {
                    stream.played = stream.played.wrapping_add(u64::from(frames));
                    changed |= 1 << index;
                }
            }
        }
        if self.hardware != Hardware::Transition {
            changed |= self.schedule(output);
        }
        for index in each_bit(changed) {
            if let Some(stream) = &self.streams[index] {
                stream.publish();
            }
        }
    }

    /// 补填、按需启停物理流并检出欠载；返回位置或状态变化的流。
    fn schedule(&mut self, output: &dyn PcmOutput) -> u8 {
        // 1. 先补填：设备可一次归还多个 TX，暂时没有在途 period 不代表使用者断粮。
        let mut changed = match self.fill(output) {
            Ok(changed) => changed,
            Err(()) => return self.fail(),
        };
        // 2. 补填后仍为空才停止并 prepare；先停止会打断设备已接收的连续 PCM，造成反复停启和卡顿。
        if self.endpoint == Endpoint::Shared
            && self.hardware == Hardware::Running
            && self.queue.len == 0
        {
            if output.stop().and_then(|()| output.prepare()).is_err() {
                return self.fail();
            }
            self.hardware = Hardware::Stopped;
        }
        // 3. 有在途数据而设备没在跑：启动。
        if self.hardware == Hardware::Stopped && self.queue.len > 0 {
            if output.start().is_err() {
                return self.fail();
            }
            self.hardware = Hardware::Running;
        }
        // 4. 播放中的流既没有可混的数据、也没有在途数据：它已经跑空。
        for (index, slot) in self.streams.iter_mut().enumerate() {
            if let Some(stream) = slot
                && stream.playing()
                && stream.pending() == 0
                && stream.mixed == stream.played
            {
                stream.underrun();
                changed |= 1 << index;
            }
        }
        changed
    }

    /// 按 [`LEAD_PERIODS`] 补填硬件队列，留出一段使用者数据跨越整批完成。
    ///
    /// # Returns
    ///
    /// 状态因此变化的流；设备失败时返回 `Err`。
    fn fill(&mut self, output: &dyn PcmOutput) -> Result<u8, ()> {
        let Some(configuration) = self.configuration else {
            return Ok(0);
        };
        let frame_bytes = configuration.frame_bytes();
        let period_frames = configuration.period_frames as u64;
        let period_bytes = configuration.period_bytes();
        let mut changed = 0u8;
        // submit_period 在设备 owner 内原子准入；先查询 writable 既不能预留 slot，又会多做一次 RPC。
        while self.queue.len < LEAD_PERIODS {
            let mut playing = 0u8;
            let mut ready = 0u8;
            for (index, slot) in self.streams.iter().enumerate() {
                if let Some(stream) = slot
                    && stream.playing()
                {
                    playing |= 1 << index;
                    // 整周期照常预送；短批次等本流在途数据耗尽才输出，避免过早插入静音间隙。
                    let pending = stream.pending();
                    if pending >= period_frames || pending != 0 && stream.mixed == stream.played {
                        ready |= 1 << index;
                    }
                }
            }
            if ready == 0 {
                break;
            }
            // 缺少未来数据的流若仍有在途 period，交付期限尚未到；先等它补充，不能提前混入缺口或报欠载。
            if each_bit(playing & !ready).any(|index| {
                self.streams[index]
                    .as_ref()
                    .is_some_and(|stream| stream.mixed != stream.played)
            }) {
                break;
            }
            // 这个 period 不含这些流的数据：它们的时间线出现了缺口，按 ALSA 语义就是 underrun。
            for index in each_bit(playing & !ready) {
                if let Some(stream) = self.streams[index].as_mut() {
                    stream.underrun();
                    changed |= 1 << index;
                }
            }
            let mut accumulator = Accumulator::new();
            let mut source = [0u8; PCM_PERIOD_BYTES];
            let mut mixed = 0u8;
            let mut contributions = [0u16; STREAMS];
            for index in each_bit(ready) {
                let Some(stream) = self.streams[index].as_mut() else {
                    continue;
                };
                let offset = (stream.mixed % stream.buffer_frames) as usize * frame_bytes;
                let frames = stream.pending().min(period_frames) as usize;
                let bytes = frames * frame_bytes;
                let first = bytes.min(stream.buffer_frames as usize * frame_bytes - offset);
                let period = if self.endpoint == Endpoint::Exclusive {
                    &mut self.period[..period_bytes]
                } else {
                    &mut source[..]
                };
                let read = stream
                    .io
                    .read(offset as u64, &mut period[..first])
                    .and_then(|()| {
                        if first < bytes {
                            stream.io.read(0, &mut period[first..bytes])
                        } else {
                            Ok(())
                        }
                    });
                if read.is_ok() {
                    if self.endpoint == Endpoint::Exclusive {
                        self.period[bytes..period_bytes].fill(0);
                    } else {
                        source[bytes..].fill(0);
                        accumulator.add(&source, stream.gain.map(volume_gain));
                    }
                    mixed |= 1 << index;
                    contributions[index] = frames as u16;
                } else {
                    // 环形缓冲读取不应失败；失败的流退出这个 period，而不是让混音带着垃圾数据出声。
                    stream.underrun();
                    changed |= 1 << index;
                }
            }
            if mixed == 0 {
                continue;
            }
            if self.endpoint == Endpoint::Shared {
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
                if let Some(stream) = self.streams[index].as_mut() {
                    stream.mixed = stream.mixed.wrapping_add(u64::from(contributions[index]));
                }
            }
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

    /// 设备永久失败：所有流断开，在途记账作废；返回全部流的掩码供调用方发布。
    pub(crate) fn fail(&mut self) -> u8 {
        self.hardware = Hardware::Failed;
        self.queue.clear();
        let mut changed = 0u8;
        for (index, slot) in self.streams.iter_mut().enumerate() {
            if let Some(stream) = slot {
                stream.state = StreamState::Disconnected;
                stream.publish();
                changed |= 1 << index;
            }
        }
        changed
    }
}
