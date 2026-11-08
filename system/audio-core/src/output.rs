//! 一个 PCM 输出：设备会话、混音引擎、混音线程与它的渲染流。
//!
//! 1. 混音线程在本输出的 port 上等待：设备完成（观察者投递的用户 packet）、流的已写入通知（`WRITTEN`）
//!    与流 channel 关闭（释放流）。每次醒来消化一个事件快照并补填设备队列。
//! 2. 格式切换（独占流的协商格式、共享格式与空闲）是冷事务：持配置锁冻结提交，在引擎锁外完成可睡眠的
//!    设备控制，再恢复提交。设备回调绝不取引擎锁。

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use audio_protocol::{
    ControlPage, Controls, CreatedStream, Endpoint, OutputInfo, PROGRESS, STATUS_DISCONNECTED,
    STATUS_PLAYING, STATUS_UNDERRUN, StreamService, WRITTEN,
};
use device_model::{
    MAX_PCM_PERIOD_BYTES, PcmCompletionObserver, PcmConfiguration, PcmOutput, PcmOutputError,
};
use native_runtime::native_abi::{self, Status, rights, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Port, Vmo};

use crate::engine::{self, Engine, Events, Master, STREAMS, Stream, StreamIo, StreamState};

/// 一个渲染流的原生数据面：使用者的环形缓冲、共享控制区、流 Event 与 channel 服务端副本。
struct RemoteStream {
    ring: Vmo,
    control: ControlPage,
    event: Event,
    // OWNER: 流 channel 的服务端副本，承载关闭等待；关闭它会撤销等待，因此随流保存到释放。
    peer: Option<Channel>,
}

impl StreamIo for RemoteStream {
    fn read(&self, offset: u64, output: &mut [u8]) -> Result<(), ()> {
        self.ring.read(output, offset).map_err(|_| ())
    }

    fn written(&self) -> u64 {
        self.control.written()
    }

    fn reset_written(&self) {
        self.control.set_written(0);
    }

    fn publish(&self, played: u64, state: StreamState) {
        self.control.set_played(played);
        self.control.set_status(match state {
            StreamState::Idle => 0,
            StreamState::Playing => STATUS_PLAYING,
            StreamState::Underrun => STATUS_UNDERRUN,
            StreamState::Disconnected => STATUS_DISCONNECTED,
        });
        let _ = self.event.signal(0, PROGRESS);
    }
}

fn endpoint(endpoint: Endpoint) -> engine::Endpoint {
    match endpoint {
        Endpoint::Shared => engine::Endpoint::Shared,
        Endpoint::Exclusive => engine::Endpoint::Exclusive,
    }
}

/// 设备完成的 port key；流的已写入通知与关闭分别从 [`STREAM_WRITTEN`] 与 [`STREAM_CLOSED`] 起编号。
const DEVICE: u64 = 1;
const STREAM_WRITTEN: u64 = 0x100;
const STREAM_CLOSED: u64 = 0x200;

/// 设备回调累计的完成；回调只发布计数并唤醒混音线程，不等待引擎锁。
#[derive(Default)]
struct EventCounters {
    completed: AtomicUsize,
    disconnected: AtomicBool,
}

impl EventCounters {
    fn take(&self) -> Events {
        Events {
            completed: self.completed.swap(0, Ordering::AcqRel),
            disconnected: self.disconnected.load(Ordering::Acquire),
        }
    }
}

/// 一个 PCM 输出。
pub(crate) struct Output {
    pub(crate) info: OutputInfo,
    device: Arc<dyn PcmOutput>,
    // OWNER: 冷配置事务的唯一锁；等待 USB 控制传输时不持引擎锁。
    transition: Mutex<()>,
    // OWNER: 引擎状态（流位置、在途 period 记账、主音量）的唯一入口；持锁期间会调用 `device`，所以设备
    // 回调绝不取它。缺少这条纪律，`submit_period` 内部触发的回调会在同一个线程上自旋等待自己。
    engine: Mutex<Engine<RemoteStream>>,
    events: EventCounters,
    port: Port,
}

impl Output {
    /// 接入一个 PCM 设备：支持共享格式时按共享格式 prepare，启动混音线程。
    ///
    /// # Errors
    ///
    /// 设备配置、port 或线程创建失败。
    pub(crate) fn attach(
        id: u64,
        device: Arc<dyn PcmOutput>,
        scheduler: Arc<native_runtime::Resource>,
    ) -> Result<Arc<Self>, Status> {
        let info = OutputInfo {
            id,
            name: device.name().as_bytes().to_vec(),
            capabilities: device.capabilities(),
        };
        let shared = info.shared_configuration();
        if let Some(configuration) = shared {
            device
                .configure(configuration)
                .and_then(|()| device.prepare())
                .map_err(|_| native_abi::ERR_IO)?;
        }
        let output = Arc::try_new(Self {
            info,
            device: device.clone(),
            transition: Mutex::new(()),
            engine: Mutex::new(Engine::new(shared)),
            events: EventCounters::default(),
            port: Port::create()?,
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        device
            .set_observer(output.clone())
            .map_err(|_| native_abi::ERR_IO)?;
        let mixer = output.clone();
        let thread = native_runtime::thread::spawn(b"audio-mixer", move || mixer.run(scheduler))?;
        drop(thread);
        Ok(output)
    }

    fn run(&self, scheduler: Arc<native_runtime::Resource>) {
        let mut configured = None;
        loop {
            let Ok(packet) = self.port.wait(native_abi::DEADLINE_INFINITE) else {
                continue;
            };
            match packet.key {
                key if (STREAM_CLOSED..STREAM_CLOSED + STREAMS as u64).contains(&key) => {
                    self.release((key - STREAM_CLOSED) as usize);
                }
                key if (STREAM_WRITTEN..STREAM_WRITTEN + STREAMS as u64).contains(&key) => {
                    self.watch_written((key - STREAM_WRITTEN) as usize);
                }
                _ => {}
            }
            if let Err(status) = self.process(&scheduler, &mut configured) {
                error!(
                    "audio output {} deadline configuration failed: {status}",
                    self.info.id
                );
                self.engine.lock().fail();
                let _ = self.device.stop();
                break;
            }
        }
    }

    /// 消化一个设备事件快照并补填队列。先取引擎锁再提取计数，避免并发调用把较新的完成批次先交给引擎。
    fn process(
        &self,
        scheduler: &native_runtime::Resource,
        configured: &mut Option<PcmConfiguration>,
    ) -> Result<(), Status> {
        let mut engine = self.engine.lock();
        let configuration = engine.configuration();
        if configuration != *configured {
            let period = configuration.map_or(0, |configuration| {
                ((configuration.period_frames as u128 * 1_000_000_000)
                    / u128::from(configuration.rate)) as u64
            });
            // 每个实际 PCM 周期预留 10% CPU；其余可供显示/公平类。配置失败显式停止输出，不能静默退回非实时混音。
            native_runtime::thread::current().handle().set_deadline(
                scheduler,
                period / 10,
                period,
                period,
            )?;
            *configured = configuration;
        }
        engine.tick(self.events.take(), &*self.device);
        Ok(())
    }

    /// RPC 只更新控制状态并通知已有混音端口，所有实际混音/提交由拥有 CBS 预算的线程执行。
    fn wake_mixer(&self) {
        let _ = self.port.queue(DEVICE, 0);
    }

    /// 清除已写入通知并重新登记等待；之后的写入不会丢失。
    fn watch_written(&self, index: usize) {
        let engine = self.engine.lock();
        if let Some(stream) = engine.stream(index) {
            let event = &stream.io().event;
            let _ = event.signal(WRITTEN, 0);
            let _ = event.wait_async(&self.port, STREAM_WRITTEN + index as u64, WRITTEN);
        }
    }

    fn release(&self, index: usize) {
        let (stream, exclusive) = {
            let mut engine = self.engine.lock();
            let exclusive = engine.endpoint() == engine::Endpoint::Exclusive;
            (engine.release(index), exclusive)
        };
        drop(stream);
        // 独占流离开：释放协商格式；输出支持共享格式时回到共享格式，供之后的共享流使用。
        if exclusive && !self.engine.lock().occupied() {
            let target = self.info.shared_configuration();
            if let Err(error) = self.switch(target) {
                warn!("audio output {} release failed: {error:?}", self.info.id);
            }
        }
    }

    /// 把物理流切换到 `target`：冻结提交，在引擎锁外 stop/release/configure/prepare，再恢复提交。
    fn switch(&self, target: Option<PcmConfiguration>) -> Result<(), PcmOutputError> {
        let _transition = self.transition.lock();
        let (previous, running) = {
            let mut engine = self.engine.lock();
            let previous = engine.configuration();
            (previous, engine.begin_transition()?)
        };
        let result = (|| {
            if running {
                self.device.stop()?;
            }
            if previous.is_some() {
                self.device.release()?;
            }
            // RELEASE 是旧 DMA 与回调的完成屏障，新的 stream 尚未提交任何 I/O。
            let _ = self.events.take();
            if let Some(configuration) = target {
                self.device.configure(configuration)?;
                self.device.prepare()?;
            }
            Ok(())
        })();
        let mut engine = self.engine.lock();
        if let Err(error) = result {
            engine.fail();
            return Err(error);
        }
        engine.finish_transition(target)
    }

    /// 主音量与静音。
    ///
    /// # Errors
    ///
    /// 设备已失败或断开（使用者据此报告声卡断开）。
    pub(crate) fn controls(&self) -> Result<Controls, Status> {
        let engine = self.engine.lock();
        if engine.failed() {
            return Err(native_abi::ERR_IO);
        }
        let master = engine.controls();
        Ok(Controls {
            volume: master.volume,
            enabled: master.enabled,
        })
    }

    pub(crate) fn set_controls(&self, controls: Controls) -> bool {
        let changed = self.engine.lock().set_controls(Master {
            volume: controls.volume,
            enabled: controls.enabled,
        });
        if changed {
            self.wake_mixer();
        }
        changed
    }

    /// 建立渲染流。
    ///
    /// # Errors
    ///
    /// 端点冲突或已满（`ERR_ALREADY_BOUND`）、格式不受支持（`ERR_NOT_SUPPORTED`）或设备失败。
    pub(crate) fn create_stream(
        self: &Arc<Self>,
        endpoint: Endpoint,
        configuration: PcmConfiguration,
        buffer_frames: u64,
        ring: Vmo,
    ) -> Result<CreatedStream, Status> {
        if buffer_frames == 0 || configuration.period_bytes() > MAX_PCM_PERIOD_BYTES {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        let physical = match endpoint {
            Endpoint::Shared if configuration == PcmConfiguration::SHARED => self
                .info
                .shared_configuration()
                .ok_or(native_abi::ERR_NOT_SUPPORTED)?,
            Endpoint::Exclusive if self.info.capabilities.supports(configuration) => configuration,
            _ => return Err(native_abi::ERR_NOT_SUPPORTED),
        };
        if !self.engine.lock().admits(self::endpoint(endpoint)) {
            return Err(native_abi::ERR_ALREADY_BOUND);
        }
        if self.engine.lock().configuration() != Some(physical) {
            self.switch(Some(physical))
                .map_err(|_| native_abi::ERR_IO)?;
        }
        let control = ControlPage::create()?;
        let client_control = control.duplicate()?;
        let event = Event::create()?;
        let client_event = event.duplicate(rights::SAME)?;
        let stream = RemoteStream {
            ring,
            control,
            event,
            peer: None,
        };
        let index = self
            .engine
            .lock()
            .allocate(self::endpoint(endpoint), Stream::new(stream, buffer_frames))
            .map_err(|_| native_abi::ERR_ALREADY_BOUND)?;
        self.watch_written(index);
        let handle = Arc::try_new(StreamHandle {
            output: Arc::downgrade(self),
            index,
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let output = self.clone();
        Ok(CreatedStream {
            stream: handle,
            control: client_control,
            event: client_event,
            opened: alloc::boxed::Box::new(move |server: Channel| {
                let key = STREAM_CLOSED + index as u64;
                if server
                    .wait_async(&output.port, key, signals::PEER_CLOSED)
                    .is_err()
                {
                    output.release(index);
                    return;
                }
                if let Some(stream) = output.engine.lock().stream_mut(index) {
                    stream.io_mut().peer = Some(server);
                }
            }),
        })
    }
}

impl PcmCompletionObserver for Output {
    fn period_completed(&self, _frames: usize) {
        self.events.completed.fetch_add(1, Ordering::AcqRel);
        let _ = self.port.queue(DEVICE, 0);
    }

    fn xrun(&self) {
        // 设备欠载的后果（队列排空）由 `tick` 通过完成记账发现；这里只让它尽快被处理。
        let _ = self.port.queue(DEVICE, 0);
    }

    fn disconnected(&self) {
        self.events.disconnected.store(true, Ordering::Release);
        let _ = self.port.queue(DEVICE, 0);
        info!("audio output reset");
    }
}

/// 一个渲染流的服务端控制。
struct StreamHandle {
    output: Weak<Output>,
    index: usize,
}

impl StreamHandle {
    fn output(&self) -> Result<Arc<Output>, Status> {
        self.output.upgrade().ok_or(native_abi::ERR_PEER_CLOSED)
    }
}

impl StreamService for StreamHandle {
    fn start(&self) -> Result<(), Status> {
        let output = self.output()?;
        output
            .engine
            .lock()
            .start(self.index)
            .map_err(|()| native_abi::ERR_BAD_STATE)?;
        output.wake_mixer();
        Ok(())
    }

    fn stop(&self) -> Result<(), Status> {
        let output = self.output()?;
        let (exclusive, configuration) = {
            let mut engine = output.engine.lock();
            engine
                .stop(self.index)
                .map_err(|()| native_abi::ERR_BAD_STATE)?;
            (
                engine.endpoint() == engine::Endpoint::Exclusive && engine.running(),
                engine.configuration(),
            )
        };
        // 独占流直通设备：停止必须同时撤销设备里已排队的 period，重新 prepare 同一格式。
        if exclusive {
            output
                .switch(configuration)
                .map_err(|_| native_abi::ERR_IO)?;
        }
        output.wake_mixer();
        Ok(())
    }

    fn set_gain(&self, gain: [u32; 2]) -> Result<(), Status> {
        let output = self.output()?;
        output
            .engine
            .lock()
            .set_gain(self.index, gain)
            .map_err(|()| native_abi::ERR_BAD_STATE)?;
        Ok(())
    }
}
