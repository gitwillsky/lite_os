#![no_std]
#![feature(allocator_ext)]

//! 音频服务协议（Fuchsia audio_core + AudioRenderer）：原生 `audio-core` 拥有 PCM 输出设备与混音，
//! 使用者（linuxd 的 ALSA 翻译）创建渲染流并经共享环形缓冲提交样本。
//!
//! 1. 服务连接：列出输出（含热插拔，[`AudioClient::watch`]）、建立渲染流、读写输出的主音量与静音。
//! 2. 渲染流：共享端点在输出的共享格式（[`PcmConfiguration::SHARED`]）上与其他流混音；独占端点直接以
//!    协商格式驱动设备，要求输出上没有其他流。样本在使用者提供的环形缓冲 VMO 中（payload buffer）。
//! 3. 热路径不走 RPC：每个流有一页共享控制区（[`ControlPage`]）。使用者发布已写入帧数并置位流 Event 的
//!    `USER_1`；服务混入后发布已播放帧数与状态并置位 `USER_0`。帧计数从 [`StreamClient::stop`] 之后清零。
//! 4. 关闭流 channel 即释放流；服务摘掉它在在途 period 中的归属。

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub use device_model::{PcmCapabilities, PcmConfiguration, PcmFormat};
use native_runtime::native_abi::{self, Status, signals};
use native_runtime::{Channel, Event, Handle, Vmo};
use service_rpc::{Reader, Writer, call, serve, take_handle};

const OUTPUTS: u32 = 1;
const WATCH: u32 = 2;
const CREATE_STREAM: u32 = 3;
const CONTROLS: u32 = 4;
const SET_CONTROLS: u32 = 5;

const START: u32 = 1;
const STOP: u32 = 2;
const SET_GAIN: u32 = 3;

/// 使用者写入帧数后置位的流 Event signal。
pub const WRITTEN: u32 = signals::USER_0 << 1;
/// 服务推进播放位置或改变状态后置位的流 Event signal。
pub const PROGRESS: u32 = signals::USER_0;

/// 流正在播放（已 start、尚未欠载或停止）。
pub const STATUS_PLAYING: u32 = 1 << 0;
/// 运行中的流没有可混的数据：时间线出现缺口（ALSA underrun）。
pub const STATUS_UNDERRUN: u32 = 1 << 1;
/// 输出设备失败或断开，流永久不可用。
pub const STATUS_DISCONNECTED: u32 = 1 << 2;
/// 音量上限（单位增益）。
pub const VOLUME_MAX: u32 = 100;

/// 一个 PCM 输出。
#[derive(Debug, Clone)]
pub struct OutputInfo {
    /// 服务内稳定的输出 ID；热插拔的新输出取更大的 ID。
    pub id: u64,
    pub name: Vec<u8>,
    pub capabilities: PcmCapabilities,
}

impl OutputInfo {
    /// 输出是否支持共享混音格式。
    pub fn shared(&self) -> bool {
        self.capabilities.supports(PcmConfiguration::SHARED)
    }
}

/// 输出的主音量与静音开关，左右声道各一份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controls {
    /// 0..=[`VOLUME_MAX`]。
    pub volume: [u32; 2],
    /// `false` 表示该声道静音。
    pub enabled: [bool; 2],
}

/// 渲染流的端点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// 与其他共享流混音。
    Shared,
    /// 独占输出，以协商格式直通设备。
    Exclusive,
}

fn encode_configuration(writer: &mut Writer, configuration: PcmConfiguration) {
    writer
        .u32(configuration.rate)
        .u8(configuration.format as u8)
        .u64(configuration.period_frames as u64);
}

fn decode_configuration(reader: &mut Reader<'_>) -> Result<PcmConfiguration, Status> {
    let rate = reader.u32()?;
    let format = *PcmFormat::ALL
        .get(usize::from(reader.u8()?))
        .ok_or(native_abi::ERR_INVALID_ARGS)?;
    let period_frames = usize::try_from(reader.u64()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    Ok(PcmConfiguration {
        rate,
        format,
        period_frames,
    })
}

/// 流的共享控制区：一页 VMO，由使用者与服务各自映射。
pub struct ControlPage {
    vmo: Vmo,
    base: usize,
}

const WRITTEN_OFFSET: usize = 0;
const PLAYED_OFFSET: usize = 8;
const STATUS_OFFSET: usize = 16;

impl ControlPage {
    /// 建立一页新的控制区并映射到本进程。
    ///
    /// # Errors
    ///
    /// VMO 创建或映射失败。
    pub fn create() -> Result<Self, Status> {
        Self::map(Vmo::create(native_abi::PAGE_SIZE as u64, 0)?)
    }

    /// 映射对端交来的控制区。
    ///
    /// # Errors
    ///
    /// 映射失败。
    pub fn map(vmo: Vmo) -> Result<Self, Status> {
        let base = native_runtime::heap::vmar().map(
            native_abi::vm::READ | native_abi::vm::WRITE,
            0,
            &vmo,
            0,
            native_abi::PAGE_SIZE,
        )?;
        Ok(Self { vmo, base })
    }

    fn u64_at(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: base 映射了一整页且永不在本对象存活期间解除；偏移 8 字节对齐且在页内。
        unsafe { &*((self.base + offset) as *const AtomicU64) }
    }

    fn u32_at(&self, offset: usize) -> &AtomicU32 {
        // SAFETY: 同上；偏移 4 字节对齐且在页内。
        unsafe { &*((self.base + offset) as *const AtomicU32) }
    }

    /// 使用者已写入的帧数（自上次停止起累计）。
    pub fn written(&self) -> u64 {
        self.u64_at(WRITTEN_OFFSET).load(Ordering::Acquire)
    }

    /// 发布已写入的帧数；样本必须先写进环形缓冲。
    pub fn set_written(&self, frames: u64) {
        self.u64_at(WRITTEN_OFFSET).store(frames, Ordering::Release);
    }

    /// 服务已播放（设备已消费）的帧数。
    pub fn played(&self) -> u64 {
        self.u64_at(PLAYED_OFFSET).load(Ordering::Acquire)
    }

    /// 服务发布已播放的帧数。
    pub fn set_played(&self, frames: u64) {
        self.u64_at(PLAYED_OFFSET).store(frames, Ordering::Release);
    }

    /// 流状态位（`STATUS_*`）。
    pub fn status(&self) -> u32 {
        self.u32_at(STATUS_OFFSET).load(Ordering::Acquire)
    }

    /// 服务发布流状态位。
    pub fn set_status(&self, status: u32) {
        self.u32_at(STATUS_OFFSET).store(status, Ordering::Release);
    }

    /// 交给对端的副本。
    ///
    /// # Errors
    ///
    /// 句柄复制失败。
    pub fn duplicate(&self) -> Result<Vmo, Status> {
        self.vmo.duplicate(native_abi::rights::SAME)
    }
}

impl Drop for ControlPage {
    fn drop(&mut self) {
        // SAFETY: 原子引用只借自 `&self`，析构时已没有借用者；映射只属于本对象。
        let _ = unsafe { native_runtime::heap::vmar().unmap(self.base, native_abi::PAGE_SIZE) };
    }
}

/// 一个已建立的渲染流（服务端交给 [`serve_audio`] 的结果）。
pub struct CreatedStream {
    /// 流控制的服务端处理者。
    pub stream: Arc<dyn StreamService>,
    /// 交给使用者的控制区副本。
    pub control: Vmo,
    /// 交给使用者的流 Event 副本。
    pub event: Event,
    /// 服务观察使用者关闭流 channel 的回调：channel 的服务端副本由服务持有。
    pub opened: alloc::boxed::Box<dyn FnOnce(Channel) + Send>,
}

/// 服务端：音频服务。
pub trait AudioService: Send + Sync {
    /// ID 大于 `after` 的输出。
    fn outputs(&self, after: u64) -> Result<Vec<OutputInfo>, Status>;
    /// 输出集合变化时置位 `USER_0` 的 Event。
    fn watch(&self) -> Result<Event, Status>;
    /// 在输出 `output` 上建立渲染流。
    ///
    /// # Errors
    ///
    /// 输出不存在（`ERR_NOT_FOUND`）、端点冲突或满（`ERR_ALREADY_BOUND`）、格式不受支持
    /// （`ERR_NOT_SUPPORTED`）或设备失败。
    fn create_stream(
        &self,
        output: u64,
        endpoint: Endpoint,
        configuration: PcmConfiguration,
        buffer_frames: u64,
        ring: Vmo,
    ) -> Result<CreatedStream, Status>;
    /// 输出的主音量与静音。
    fn controls(&self, output: u64) -> Result<Controls, Status>;
    /// 设置主音量与静音；返回是否发生了变化。
    fn set_controls(&self, output: u64, controls: Controls) -> Result<bool, Status>;
}

/// 服务端：一个渲染流。
pub trait StreamService: Send + Sync {
    /// 开始播放：之后写入的帧按时间线混入。
    fn start(&self) -> Result<(), Status>;
    /// 停止并清零帧计数；已混入设备的 period 仍会播放完，但不再计入本流。
    fn stop(&self) -> Result<(), Status>;
    /// 本流的增益（0..=[`VOLUME_MAX`]，左右声道）。
    fn set_gain(&self, gain: [u32; 2]) -> Result<(), Status>;
}

/// 服务音频连接。
///
/// # Errors
///
/// 服务线程或闭包分配失败。
pub fn serve_audio(channel: Channel, service: Arc<dyn AudioService>) -> Result<(), Status> {
    serve(
        channel,
        1,
        b"audio-service",
        Arc::try_new(
            move |ordinal: u32,
                  request: &mut Reader<'_>,
                  mut handles: Vec<Handle>,
                  reply: &mut Writer| match ordinal {
                OUTPUTS => {
                    let outputs = service.outputs(request.u64()?)?;
                    reply.u32(outputs.len() as u32);
                    for output in outputs {
                        reply.u64(output.id).bytes(&output.name);
                        for rates in output.capabilities.rates {
                            reply.u32(rates);
                        }
                    }
                    Ok(Vec::new())
                }
                WATCH => Ok(alloc::vec![service.watch()?.into_handle()]),
                CREATE_STREAM => {
                    let output = request.u64()?;
                    let endpoint = if request.bool()? {
                        Endpoint::Exclusive
                    } else {
                        Endpoint::Shared
                    };
                    let configuration = decode_configuration(request)?;
                    let buffer_frames = request.u64()?;
                    let ring = Vmo::from_handle(take_handle(&mut handles, 0)?);
                    let created = service.create_stream(
                        output,
                        endpoint,
                        configuration,
                        buffer_frames,
                        ring,
                    )?;
                    let (client, server) = Channel::create()?;
                    let stream = created.stream;
                    let watched = server.duplicate(native_abi::rights::SAME)?;
                    serve(
                        server,
                        1,
                        b"audio-stream",
                        Arc::try_new(
                            move |ordinal: u32,
                                  request: &mut Reader<'_>,
                                  _: Vec<Handle>,
                                  _: &mut Writer| {
                                match ordinal {
                                    START => stream.start()?,
                                    STOP => stream.stop()?,
                                    SET_GAIN => {
                                        stream.set_gain([request.u32()?, request.u32()?])?
                                    }
                                    _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                                }
                                Ok(Vec::new())
                            },
                        )
                        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
                    )?;
                    (created.opened)(watched);
                    Ok(alloc::vec![
                        client.into_handle(),
                        created.control.into_handle(),
                        created.event.into_handle()
                    ])
                }
                CONTROLS => {
                    let controls = service.controls(request.u64()?)?;
                    reply
                        .u32(controls.volume[0])
                        .u32(controls.volume[1])
                        .bool(controls.enabled[0])
                        .bool(controls.enabled[1]);
                    Ok(Vec::new())
                }
                SET_CONTROLS => {
                    let output = request.u64()?;
                    let controls = Controls {
                        volume: [request.u32()?, request.u32()?],
                        enabled: [request.bool()?, request.bool()?],
                    };
                    reply.bool(service.set_controls(output, controls)?);
                    Ok(Vec::new())
                }
                _ => Err(native_abi::ERR_NOT_SUPPORTED),
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )
}

/// 使用者端：音频服务连接。
pub struct AudioClient(Channel);

impl AudioClient {
    pub fn new(channel: Channel) -> Self {
        Self(channel)
    }

    /// ID 大于 `after` 的输出。
    ///
    /// # Errors
    ///
    /// 连接已断开或回复格式错误。
    pub fn outputs(&self, after: u64) -> Result<Vec<OutputInfo>, Status> {
        let mut request = Writer::request(OUTPUTS);
        request.u64(after);
        let reply = call(&self.0, &request, Vec::new())?;
        let mut reader = reply.reader();
        let count = reader.u32()? as usize;
        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(count)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for _ in 0..count {
            let id = reader.u64()?;
            let name = reader.bytes()?.to_vec();
            let mut capabilities = PcmCapabilities::EMPTY;
            for rates in &mut capabilities.rates {
                *rates = reader.u32()?;
            }
            outputs.push(OutputInfo {
                id,
                name,
                capabilities,
            });
        }
        Ok(outputs)
    }

    /// 阻塞到服务端关闭连接（audio-core 终止）；使用者据此重连新实例。
    pub fn wait_closed(&self) {
        let _ = self
            .0
            .wait_one(signals::PEER_CLOSED, native_abi::DEADLINE_INFINITE);
    }

    /// 输出集合变化时置位 `USER_0` 的 Event。
    ///
    /// # Errors
    ///
    /// 连接已断开。
    pub fn watch(&self) -> Result<Event, Status> {
        let mut reply = call(&self.0, &Writer::request(WATCH), Vec::new())?;
        Ok(Event::from_handle(take_handle(&mut reply.handles, 0)?))
    }

    /// 在输出上建立渲染流，样本在 `ring` 中。
    ///
    /// # Errors
    ///
    /// 见 [`AudioService::create_stream`]。
    pub fn create_stream(
        &self,
        output: u64,
        endpoint: Endpoint,
        configuration: PcmConfiguration,
        buffer_frames: u64,
        ring: Vmo,
    ) -> Result<StreamClient, Status> {
        let mut request = Writer::request(CREATE_STREAM);
        request.u64(output).bool(endpoint == Endpoint::Exclusive);
        encode_configuration(&mut request, configuration);
        request.u64(buffer_frames);
        let mut reply = call(&self.0, &request, alloc::vec![ring.into_handle()])?;
        let channel = Channel::from_handle(take_handle(&mut reply.handles, 0)?);
        let control = ControlPage::map(Vmo::from_handle(take_handle(&mut reply.handles, 1)?))?;
        let event = Event::from_handle(take_handle(&mut reply.handles, 2)?);
        Ok(StreamClient {
            channel,
            control,
            event,
        })
    }

    /// 输出的主音量与静音。
    ///
    /// # Errors
    ///
    /// 输出不存在或连接已断开。
    pub fn controls(&self, output: u64) -> Result<Controls, Status> {
        let mut request = Writer::request(CONTROLS);
        request.u64(output);
        let reply = call(&self.0, &request, Vec::new())?;
        let mut reader = reply.reader();
        Ok(Controls {
            volume: [reader.u32()?, reader.u32()?],
            enabled: [reader.bool()?, reader.bool()?],
        })
    }

    /// 设置主音量与静音；返回是否发生了变化。
    ///
    /// # Errors
    ///
    /// 输出不存在或连接已断开。
    pub fn set_controls(&self, output: u64, controls: Controls) -> Result<bool, Status> {
        let mut request = Writer::request(SET_CONTROLS);
        request
            .u64(output)
            .u32(controls.volume[0])
            .u32(controls.volume[1])
            .bool(controls.enabled[0])
            .bool(controls.enabled[1]);
        let reply = call(&self.0, &request, Vec::new())?;
        reply.reader().bool()
    }
}

/// 使用者端：一个渲染流。关闭即释放。
pub struct StreamClient {
    channel: Channel,
    control: ControlPage,
    event: Event,
}

impl StreamClient {
    /// 共享控制区。
    pub fn control(&self) -> &ControlPage {
        &self.control
    }

    /// 流 Event：使用者等待 [`PROGRESS`]，写入后置位 [`WRITTEN`]。
    pub fn event(&self) -> &Event {
        &self.event
    }

    /// 服务端已关闭流（audio-core 终止或输出移除）：流永久不可用。
    pub fn closed(&self) -> bool {
        self.channel.wait_one(signals::PEER_CLOSED, 0).is_ok()
    }

    /// 发布已写入帧数并通知服务。
    pub fn commit(&self, written: u64) {
        self.control.set_written(written);
        let _ = self.event.signal(0, WRITTEN);
    }

    /// 开始播放。
    ///
    /// # Errors
    ///
    /// 流已断开。
    pub fn start(&self) -> Result<(), Status> {
        call(&self.channel, &Writer::request(START), Vec::new()).map(drop)
    }

    /// 停止并清零帧计数。
    ///
    /// # Errors
    ///
    /// 流已断开。
    pub fn stop(&self) -> Result<(), Status> {
        call(&self.channel, &Writer::request(STOP), Vec::new()).map(drop)
    }

    /// 设置本流增益。
    ///
    /// # Errors
    ///
    /// 流已断开。
    pub fn set_gain(&self, gain: [u32; 2]) -> Result<(), Status> {
        let mut request = Writer::request(SET_GAIN);
        request.u32(gain[0]).u32(gain[1]);
        call(&self.channel, &request, Vec::new()).map(drop)
    }
}
