//! PCM 播放：控制请求与 period 数据走请求 channel，period 完成、xrun 与断开经事件 channel 回调。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::{
    PcmCapabilities, PcmCompletionObserver, PcmConfiguration, PcmFormat, PcmOutput, PcmOutputError,
};
use native_runtime::native_abi::{self, Status};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Message};

use crate::directory::{self, Announcement, Class};
use crate::{EventHub, Reader, Writer, call, serve, take_handle};

const CONFIGURE: u32 = 1;
const RELEASE: u32 = 2;
const PREPARE: u32 = 3;
const START: u32 = 4;
const STOP: u32 = 5;
const SUBMIT: u32 = 6;
const WRITABLE: u32 = 7;

const EVENT_PERIOD: u32 = 1;
const EVENT_XRUN: u32 = 2;
const EVENT_DISCONNECTED: u32 = 3;

const ERROR_WOULD_BLOCK: Status = native_abi::ERR_SHOULD_WAIT;
const ERROR_INVALID_STATE: Status = native_abi::ERR_BAD_STATE;

fn encode_error(error: PcmOutputError) -> Status {
    match error {
        PcmOutputError::WouldBlock => ERROR_WOULD_BLOCK,
        PcmOutputError::InvalidState => ERROR_INVALID_STATE,
        PcmOutputError::Device => native_abi::ERR_IO,
    }
}

fn decode_error(status: Status) -> PcmOutputError {
    match status {
        ERROR_WOULD_BLOCK => PcmOutputError::WouldBlock,
        ERROR_INVALID_STATE => PcmOutputError::InvalidState,
        _ => PcmOutputError::Device,
    }
}

fn format(value: u8) -> Result<PcmFormat, Status> {
    PcmFormat::ALL.get(usize::from(value)).copied().ok_or(native_abi::ERR_INVALID_ARGS)
}

/// 驱动宿主侧观察者：把回调写成事件消息。
struct EventWriter(Channel);

impl EventWriter {
    fn send(&self, kind: u32, frames: u64) {
        let mut message = directory::description();
        message.u32(kind).u64(frames);
        let _ = self.0.write(message.as_bytes(), Vec::new());
    }
}

impl PcmCompletionObserver for EventWriter {
    fn period_completed(&self, frames: usize) {
        self.send(EVENT_PERIOD, frames as u64);
    }

    fn xrun(&self) {
        self.send(EVENT_XRUN, 0);
    }

    fn disconnected(&self) {
        self.send(EVENT_DISCONNECTED, 0);
    }
}

/// 驱动宿主：发布 PCM 输出。
///
/// # Errors
///
/// 对象、线程创建、观察者登记或目录写入失败。
pub fn publish(directory: &Channel, device: Arc<dyn PcmOutput>) -> Result<(), Status> {
    let (client, server) = Channel::create()?;
    let (events_client, events_server) = Channel::create()?;
    let observer: Arc<dyn PcmCompletionObserver> =
        Arc::try_new(EventWriter(events_server)).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    device.set_observer(observer).map_err(encode_error)?;
    let mut description = directory::description();
    description.bytes(device.name().as_bytes());
    for rates in device.capabilities().rates {
        description.u32(rates);
    }
    serve(
        server,
        1,
        b"pcm-server",
        Arc::try_new(move |ordinal: u32, request: &mut Reader<'_>, _, reply: &mut Writer| {
            match ordinal {
                CONFIGURE => {
                    let configuration = PcmConfiguration {
                        rate: request.u32()?,
                        format: format(request.u8()?)?,
                        period_frames: request.usize()?,
                    };
                    device.configure(configuration).map_err(encode_error)?;
                }
                RELEASE => device.release().map_err(encode_error)?,
                PREPARE => device.prepare().map_err(encode_error)?,
                START => device.start().map_err(encode_error)?,
                STOP => device.stop().map_err(encode_error)?,
                SUBMIT => device.submit_period(request.bytes()?).map_err(encode_error)?,
                WRITABLE => {
                    reply.bool(device.writable());
                }
                _ => return Err(native_abi::ERR_NOT_SUPPORTED),
            }
            Ok(Vec::new())
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?;
    directory::announce(
        directory,
        Class::Pcm,
        &description,
        alloc::vec![client.into_handle(), events_client.into_handle()],
    )
}

/// 使用者端的远端 PCM 输出。
pub struct RemotePcm {
    channel: Channel,
    name: String,
    capabilities: PcmCapabilities,
    // OWNER: 使用者登记的观察者；事件 channel 的分派线程调用它。缺失时 period 完成无法推进 ALSA 指针。
    observer: Arc<Mutex<Option<Arc<dyn PcmCompletionObserver>>>>,
}

/// 连接目录中的 PCM 输出。
///
/// # Errors
///
/// 描述或句柄不完整，或事件登记失败。
pub fn connect(mut announcement: Announcement, hub: &Arc<EventHub>) -> Result<Arc<RemotePcm>, Status> {
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    let events = Channel::from_handle(take_handle(&mut announcement.handles, 1)?);
    let mut reader = announcement.reader();
    let name = String::from_utf8(reader.bytes()?.to_vec()).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let mut capabilities = PcmCapabilities::EMPTY;
    for rates in &mut capabilities.rates {
        *rates = reader.u32()?;
    }
    let observer: Arc<Mutex<Option<Arc<dyn PcmCompletionObserver>>>> =
        Arc::try_new(Mutex::new(None)).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let dispatch = observer.clone();
    hub.events(
        events,
        Box::try_new(move |message: Message| {
            let mut reader = Reader::new(&message.bytes);
            let (Ok(kind), Ok(frames)) = (reader.u32(), reader.u64()) else {
                return;
            };
            let Some(observer) = dispatch.lock().clone() else {
                return;
            };
            match kind {
                EVENT_PERIOD => observer.period_completed(frames as usize),
                EVENT_XRUN => observer.xrun(),
                EVENT_DISCONNECTED => observer.disconnected(),
                _ => {}
            }
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?;
    Arc::try_new(RemotePcm {
        channel,
        name,
        capabilities,
        observer,
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}

impl RemotePcm {
    fn simple(&self, ordinal: u32) -> Result<(), PcmOutputError> {
        call(&self.channel, &Writer::request(ordinal), Vec::new()).map_err(decode_error)?;
        Ok(())
    }
}

impl PcmOutput for RemotePcm {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> PcmCapabilities {
        self.capabilities
    }

    fn set_observer(&self, observer: Arc<dyn PcmCompletionObserver>) -> Result<(), PcmOutputError> {
        *self.observer.lock() = Some(observer);
        Ok(())
    }

    fn configure(&self, configuration: PcmConfiguration) -> Result<(), PcmOutputError> {
        let mut request = Writer::request(CONFIGURE);
        request
            .u32(configuration.rate)
            .u8(configuration.format as u8)
            .u64(configuration.period_frames as u64);
        call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        Ok(())
    }

    fn release(&self) -> Result<(), PcmOutputError> {
        self.simple(RELEASE)
    }

    fn prepare(&self) -> Result<(), PcmOutputError> {
        self.simple(PREPARE)
    }

    fn start(&self) -> Result<(), PcmOutputError> {
        self.simple(START)
    }

    fn stop(&self) -> Result<(), PcmOutputError> {
        self.simple(STOP)
    }

    fn submit_period(&self, bytes: &[u8]) -> Result<(), PcmOutputError> {
        let mut request = Writer::request(SUBMIT);
        request.bytes(bytes);
        call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        Ok(())
    }

    fn writable(&self) -> bool {
        call(&self.channel, &Writer::request(WRITABLE), Vec::new())
            .and_then(|reply| reply.reader().bool())
            .unwrap_or(false)
    }
}
