//! PCM 播放：控制请求与 period 数据走请求 channel，period 完成、xrun 与断开经事件 channel 回调。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::{
    PCM_PERIODS, PcmCapabilities, PcmCompletionObserver, PcmConfiguration, PcmFormat, PcmOutput,
    PcmOutputError,
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
    PcmFormat::ALL
        .get(usize::from(value))
        .copied()
        .ok_or(native_abi::ERR_INVALID_ARGS)
}

/// 驱动宿主侧观察者：把回调写成事件消息。
struct EventWriter {
    // OWNER: 事件端点和代际的唯一事务；发送失败撤销端点，让接收者观察终止而非永久丢失额度。
    // 冷控制只在旧 RELEASE 回调屏障后换代，不在此锁内执行设备操作。
    state: Mutex<EventWriterState>,
}

struct EventWriterState {
    channel: Option<Channel>,
    generation: u64,
}

impl EventWriter {
    fn generation(&self) -> u64 {
        self.state.lock().generation
    }

    fn set_generation(&self, generation: u64) -> Result<(), Status> {
        let mut state = self.state.lock();
        if state.channel.is_none() {
            return Err(native_abi::ERR_PEER_CLOSED);
        }
        state.generation = generation;
        Ok(())
    }

    fn send(&self, kind: u32, frames: u64) {
        let retired = {
            let mut state = self.state.lock();
            let Some(channel) = &state.channel else {
                return;
            };
            let mut message = directory::description();
            message.u64(state.generation).u32(kind).u64(frames);
            if channel.write(message.as_bytes(), Vec::new()).is_err() {
                state.channel.take()
            } else {
                None
            }
        };
        // 关闭可能唤醒 peer，必须在事件状态锁外执行；旧队列排空后 EventHub 交付终止错误。
        drop(retired);
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
    let observer = Arc::try_new(EventWriter {
        state: Mutex::new(EventWriterState {
            channel: Some(events_server),
            generation: 0,
        }),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    device
        .set_observer(observer.clone())
        .map_err(encode_error)?;
    let mut description = directory::description();
    description.bytes(device.name().as_bytes());
    for rates in device.capabilities().rates {
        description.u32(rates);
    }
    serve(
        server,
        1,
        b"pcm-server",
        Arc::try_new(
            move |ordinal: u32, request: &mut Reader<'_>, _, _: &mut Writer| {
                match ordinal {
                    CONFIGURE => {
                        let generation = request.u64()?;
                        if generation <= observer.generation() {
                            return Err(native_abi::ERR_INVALID_ARGS);
                        }
                        let configuration = PcmConfiguration {
                            rate: request.u32()?,
                            format: format(request.u8()?)?,
                            period_frames: request.usize()?,
                        };
                        device.configure(configuration).map_err(encode_error)?;
                        observer.set_generation(generation)?;
                    }
                    RELEASE => device.release().map_err(encode_error)?,
                    PREPARE => device.prepare().map_err(encode_error)?,
                    START => device.start().map_err(encode_error)?,
                    STOP => device.stop().map_err(encode_error)?,
                    SUBMIT => {
                        // 客户端额度覆盖 channel 中及设备中的全部 period；这里拒绝已准入数据表示
                        // 设备失败或违反队列容量契约，必须异步断开，不能静默丢弃已接受的数据。
                        let result = (|| {
                            let generation = request.u64()?;
                            if generation != observer.generation() {
                                return Err(native_abi::ERR_BAD_STATE);
                            }
                            device.submit_period(request.bytes()?).map_err(encode_error)
                        })();
                        if result.is_err() {
                            observer.disconnected();
                        }
                    }
                    _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                }
                Ok(Vec::new())
            },
        )
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
    // OWNER: 冷控制事务；关闭准入后才发送控制请求，防止先预留的旧提交越过 RELEASE。
    // 不由事件线程取得，也不用于数据提交，慢控制不会阻塞 EventHub。
    control: Mutex<()>,
    // OWNER: 准入代际、在途额度与观察者的唯一事务；事件先归还额度，再在锁外通知混音引擎。
    state: Arc<Mutex<ClientState>>,
}

struct ClientState {
    generation: u64,
    configuration: Option<PcmConfiguration>,
    available: usize,
    // FLAG: 冷事务期间关闭数据准入；否则旧数据可能排到释放/重配之后。
    accepting: bool,
    // FLAG: 断开不可逆；缺失时异步失败之后仍会接受并丢弃新的音频数据。
    failed: bool,
    observer: Option<Arc<dyn PcmCompletionObserver>>,
}

impl ClientState {
    fn new() -> Self {
        Self {
            generation: 0,
            configuration: None,
            available: 0,
            accepting: false,
            failed: false,
            observer: None,
        }
    }

    fn reserve(&mut self, bytes: usize) -> Result<(), PcmOutputError> {
        if self.failed {
            return Err(PcmOutputError::Device);
        }
        if !self.accepting
            || self
                .configuration
                .is_none_or(|configuration| configuration.period_bytes() != bytes)
        {
            return Err(PcmOutputError::InvalidState);
        }
        if self.available == 0 {
            return Err(PcmOutputError::WouldBlock);
        }
        self.available -= 1;
        Ok(())
    }

    fn event(&mut self, generation: u64, kind: u32, frames: u64) -> Option<u32> {
        if self.failed {
            return None;
        }
        // 物理断开不依赖流代际；即使当前未配置也永久撤销本连接。
        if kind == EVENT_DISCONNECTED {
            self.failed = true;
            self.accepting = false;
            return Some(kind);
        }
        let configuration = self.configuration?;
        if generation != self.generation {
            return None;
        }
        match kind {
            EVENT_PERIOD => {
                if self.available == PCM_PERIODS || frames != configuration.period_frames as u64 {
                    self.failed = true;
                    self.accepting = false;
                    return Some(EVENT_DISCONNECTED);
                }
                self.available += 1;
                Some(kind)
            }
            EVENT_XRUN => Some(kind),
            _ => None,
        }
    }
}

/// 连接目录中的 PCM 输出。
///
/// # Errors
///
/// 描述或句柄不完整，或事件登记失败。
pub fn connect(
    mut announcement: Announcement,
    hub: &Arc<EventHub>,
) -> Result<Arc<RemotePcm>, Status> {
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    let events = Channel::from_handle(take_handle(&mut announcement.handles, 1)?);
    let mut reader = announcement.reader();
    let name =
        String::from_utf8(reader.bytes()?.to_vec()).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let mut capabilities = PcmCapabilities::EMPTY;
    for rates in &mut capabilities.rates {
        *rates = reader.u32()?;
    }
    let state =
        Arc::try_new(Mutex::new(ClientState::new())).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let dispatch = state.clone();
    hub.events(
        events,
        Box::try_new(move |message: Result<Message, Status>| {
            let event = message.and_then(|message| {
                let mut reader = Reader::new(&message.bytes);
                Ok((reader.u64()?, reader.u32()?, reader.u64()?))
            });
            // 没有 event peer 就再也不会归还额度；格式错误同样不能静默吞掉一次硬件完成。
            let (generation, kind, frames) = event.unwrap_or((0, EVENT_DISCONNECTED, 0));
            let (kind, observer) = {
                let mut state = dispatch.lock();
                let Some(kind) = state.event(generation, kind, frames) else {
                    return;
                };
                (kind, state.observer.clone())
            };
            let Some(observer) = observer else {
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
        control: Mutex::new(()),
        state,
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}

impl RemotePcm {
    fn simple(&self, ordinal: u32) -> Result<(), PcmOutputError> {
        let _control = self.control.lock();
        let accepting = {
            let mut state = self.state.lock();
            if state.failed {
                return Err(PcmOutputError::Device);
            }
            let accepting = state.accepting;
            state.accepting = false;
            accepting
        };
        let result = call(&self.channel, &Writer::request(ordinal), Vec::new())
            .map(drop)
            .map_err(decode_error);
        let mut state = self.state.lock();
        if state.failed {
            return Err(PcmOutputError::Device);
        }
        if result.is_ok() {
            if ordinal == RELEASE {
                // 旧事件可能仍在独立 event channel；清掉配置后不再归还其额度或通知新引擎。
                state.configuration = None;
                state.available = 0;
            }
            state.accepting = matches!(ordinal, PREPARE | START);
        } else {
            state.accepting = accepting;
        }
        result
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
        let mut state = self.state.lock();
        if state.failed || state.observer.is_some() {
            return Err(PcmOutputError::Device);
        }
        state.observer = Some(observer);
        Ok(())
    }

    fn configure(&self, configuration: PcmConfiguration) -> Result<(), PcmOutputError> {
        let _control = self.control.lock();
        let generation = {
            let mut state = self.state.lock();
            if state.failed {
                return Err(PcmOutputError::Device);
            }
            if state.configuration.is_some() {
                return Err(PcmOutputError::InvalidState);
            }
            state.generation = state
                .generation
                .checked_add(1)
                .ok_or(PcmOutputError::Device)?;
            state.generation
        };
        let mut request = Writer::request(CONFIGURE);
        request
            .u64(generation)
            .u32(configuration.rate)
            .u8(configuration.format as u8)
            .u64(configuration.period_frames as u64);
        call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        let mut state = self.state.lock();
        if state.failed {
            return Err(PcmOutputError::Device);
        }
        state.configuration = Some(configuration);
        state.available = PCM_PERIODS;
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
        let mut state = self.state.lock();
        state.reserve(bytes.len())?;
        let mut request = Writer::request(SUBMIT);
        request.u64(state.generation).bytes(bytes);
        // 准入与非阻塞 channel write 同属 state 事务，控制请求不能越过已预留的旧消息。
        // 只有硬件完成会归还成功提交的额度；不在混音锁内等待设备回复。
        self.channel
            .write(request.as_bytes(), Vec::new())
            .map_err(|_| {
                // write 失败没有发表消息；本事务仍持锁，唯一归还刚预留的额度。
                state.available += 1;
                PcmOutputError::Device
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared() -> ClientState {
        ClientState {
            generation: 7,
            configuration: Some(PcmConfiguration::SHARED),
            available: PCM_PERIODS,
            accepting: true,
            failed: false,
            observer: None,
        }
    }

    #[test]
    fn admission_waits_for_matching_completion_and_rejects_wrong_period_without_spending_credit() {
        let mut state = prepared();
        let bytes = PcmConfiguration::SHARED.period_bytes();
        let frames = PcmConfiguration::SHARED.period_frames as u64;
        for _ in 0..PCM_PERIODS {
            assert_eq!(state.reserve(bytes), Ok(()));
        }
        assert_eq!(state.reserve(bytes), Err(PcmOutputError::WouldBlock));
        assert_eq!(state.event(6, EVENT_PERIOD, frames), None);
        assert_eq!(state.event(7, EVENT_XRUN, 0), Some(EVENT_XRUN));
        assert_eq!(state.reserve(bytes), Err(PcmOutputError::WouldBlock));
        assert_eq!(state.event(7, EVENT_PERIOD, frames), Some(EVENT_PERIOD));
        assert_eq!(state.reserve(bytes - 1), Err(PcmOutputError::InvalidState));
        assert_eq!(state.reserve(bytes), Ok(()));
        assert_eq!(state.reserve(bytes), Err(PcmOutputError::WouldBlock));
    }

    #[test]
    fn duplicate_or_malformed_completion_permanently_revokes_admission() {
        let bytes = PcmConfiguration::SHARED.period_bytes();
        let frames = PcmConfiguration::SHARED.period_frames as u64;
        for malformed in [false, true] {
            let mut state = prepared();
            if malformed {
                state.reserve(bytes).unwrap();
            }
            assert_eq!(
                state.event(7, EVENT_PERIOD, frames - u64::from(malformed)),
                Some(EVENT_DISCONNECTED)
            );
            assert_eq!(state.reserve(bytes), Err(PcmOutputError::Device));
            assert_eq!(state.event(7, EVENT_PERIOD, frames), None);
        }
    }

    #[test]
    fn physical_disconnect_revokes_an_unconfigured_connection() {
        let mut state = ClientState::new();
        assert_eq!(
            state.event(0, EVENT_DISCONNECTED, 0),
            Some(EVENT_DISCONNECTED)
        );
        assert_eq!(state.reserve(0), Err(PcmOutputError::Device));
    }
}
