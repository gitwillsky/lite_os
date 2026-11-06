//! evdev 输入设备：静态能力随目录记录一次传递，事件按批拉取。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::{Activity, InputAbsInfo, InputDevice, InputDeviceError, InputId, RawInputEvent};
use native_runtime::native_abi::{self, Status, rights};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event};

use crate::directory::{self, Class, Directory, OpenedDevice, Session, SessionState};
use crate::{ActivityBinding, EventHub, Reader, Writer, activity_signal, call, take_handle};

const RECEIVE: u32 = 1;
const HAS_PENDING: u32 = 2;
/// Linux `EV_CNT`：事件类型数。
const EVENT_TYPES: u16 = 0x20;
/// Linux `ABS_CNT`：绝对轴数。
const ABS_AXES: u16 = 0x40;
/// 单次拉取的最多事件数。
const BATCH: usize = 64;

fn put_abs(description: &mut Writer, info: Option<InputAbsInfo>) {
    match info {
        Some(info) => {
            description
                .bool(true)
                .i32(info.minimum)
                .i32(info.maximum)
                .i32(info.fuzz)
                .i32(info.flat)
                .i32(info.resolution);
        }
        None => {
            description.bool(false);
        }
    }
}

/// 驱动宿主：登记输入设备（独占：事件流只交给一个使用者）。
///
/// 一次拉取请求把驱动当前可取的事件（至多 [`BATCH`] 个）一并取出，随后由驱动宿主自己结束本批
/// （重新投递接收缓冲），使用者端的 `finish_receive_batch` 因此不需要往返。
///
/// # Errors
///
/// 对象创建或分配失败。
pub fn publish(directory: &Directory, device: Arc<dyn InputDevice>) -> Result<(), Status> {
    let event = Event::create()?;
    let mut description = directory::description();
    let id = device.id();
    description
        .bytes(device.name())
        .bytes(device.physical_path())
        .bytes(device.serial())
        .u16(id.bustype)
        .u16(id.vendor)
        .u16(id.product)
        .u16(id.version)
        .bytes(device.properties())
        .bytes(device.event_types());
    for event_type in 0..EVENT_TYPES {
        description.bytes(device.event_codes(event_type));
    }
    for code in 0..ABS_AXES {
        put_abs(&mut description, device.abs_info(code));
    }
    let event = Arc::try_new(event).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    device.bind_activity(activity_signal(event.clone())?);
    directory
        .add(
            Class::Input,
            &description,
            true,
            Box::new(move |end| {
                let session = SessionState::new(device.clone(), end);
                let (client, server) = Channel::create()?;
                let server = directory::serve_session(
                    server,
                    1,
                    b"input-server",
                    Arc::try_new(
                        move |ordinal: u32, _: &mut Reader<'_>, _, reply: &mut Writer| {
                            let device = session.get();
                            match ordinal {
                                RECEIVE => {
                                    let mut events = Vec::new();
                                    while events.len() < BATCH {
                                        match device
                                            .receive_event()
                                            .map_err(|_| native_abi::ERR_IO)?
                                        {
                                            Some(event) => events.push(event),
                                            None => break,
                                        }
                                    }
                                    if !events.is_empty() {
                                        device
                                            .finish_receive_batch()
                                            .map_err(|_| native_abi::ERR_IO)?;
                                    }
                                    reply.u32(events.len() as u32);
                                    for event in events {
                                        reply
                                            .u16(event.event_type)
                                            .u16(event.code)
                                            .i32(event.value);
                                    }
                                }
                                HAS_PENDING => {
                                    reply.bool(device.has_pending_event());
                                }
                                _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                            }
                            Ok(Vec::new())
                        },
                    )
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?,
                )?;
                Ok(Session {
                    handles: alloc::vec![
                        client.into_handle(),
                        event.duplicate(rights::SAME)?.into_handle()
                    ],
                    server,
                })
            }),
        )
        .map(drop)
}

/// 使用者端的远端输入设备。
pub struct RemoteInput {
    channel: Channel,
    activity: ActivityBinding,
    name: Vec<u8>,
    physical_path: Vec<u8>,
    serial: Vec<u8>,
    id: InputId,
    properties: Vec<u8>,
    event_types: Vec<u8>,
    event_codes: Vec<Vec<u8>>,
    abs_info: Vec<Option<InputAbsInfo>>,
    // OWNER: 已拉取、尚未交给 evdev 的事件；缺失时一次拉取中多余的事件会丢失。
    pending: Mutex<VecDeque<RawInputEvent>>,
}

/// 连接目录中的输入设备。
///
/// # Errors
///
/// 描述或句柄不完整，或事件登记失败。
pub fn connect(
    mut announcement: OpenedDevice,
    hub: &Arc<EventHub>,
) -> Result<Arc<RemoteInput>, Status> {
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    let activity = hub.activity(Event::from_handle(take_handle(
        &mut announcement.handles,
        1,
    )?))?;
    let mut reader = announcement.reader();
    let name = reader.bytes()?.to_vec();
    let physical_path = reader.bytes()?.to_vec();
    let serial = reader.bytes()?.to_vec();
    let id = InputId {
        bustype: reader.u16()?,
        vendor: reader.u16()?,
        product: reader.u16()?,
        version: reader.u16()?,
    };
    let properties = reader.bytes()?.to_vec();
    let event_types = reader.bytes()?.to_vec();
    let mut event_codes = Vec::new();
    for _ in 0..EVENT_TYPES {
        event_codes.push(reader.bytes()?.to_vec());
    }
    let mut abs_info = Vec::new();
    for _ in 0..ABS_AXES {
        abs_info.push(if reader.bool()? {
            Some(InputAbsInfo {
                minimum: reader.i32()?,
                maximum: reader.i32()?,
                fuzz: reader.i32()?,
                flat: reader.i32()?,
                resolution: reader.i32()?,
            })
        } else {
            None
        });
    }
    // 会话断开时活动 Event 不再置位：关闭通知让使用者再拉取一次并经 `Disconnected` 撤下设备。
    hub.on_closed(channel.duplicate(rights::SAME)?, &activity)?;
    Arc::try_new(RemoteInput {
        channel,
        activity,
        name,
        physical_path,
        serial,
        id,
        properties,
        event_types,
        event_codes,
        abs_info,
        pending: Mutex::new(VecDeque::new()),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}

impl RemoteInput {
    fn fetch(&self, pending: &mut VecDeque<RawInputEvent>) -> Result<(), InputDeviceError> {
        let reply =
            call(&self.channel, &Writer::request(RECEIVE), Vec::new()).map_err(|status| {
                if status == native_abi::ERR_PEER_CLOSED {
                    InputDeviceError::Disconnected
                } else {
                    InputDeviceError::Device
                }
            })?;
        let mut reader = reply.reader();
        let count = reader.u32().map_err(|_| InputDeviceError::Device)?;
        pending
            .try_reserve(count as usize)
            .map_err(|_| InputDeviceError::Device)?;
        for _ in 0..count {
            let decode = |reader: &mut Reader<'_>| -> Result<RawInputEvent, Status> {
                Ok(RawInputEvent {
                    event_type: reader.u16()?,
                    code: reader.u16()?,
                    value: reader.i32()?,
                })
            };
            pending.push_back(decode(&mut reader).map_err(|_| InputDeviceError::Device)?);
        }
        Ok(())
    }
}

impl InputDevice for RemoteInput {
    fn bind_activity(&self, activity: Activity) {
        self.activity.bind(activity);
    }

    fn name(&self) -> &[u8] {
        &self.name
    }

    fn physical_path(&self) -> &[u8] {
        &self.physical_path
    }

    fn serial(&self) -> &[u8] {
        &self.serial
    }

    fn id(&self) -> InputId {
        self.id
    }

    fn properties(&self) -> &[u8] {
        &self.properties
    }

    fn event_types(&self) -> &[u8] {
        &self.event_types
    }

    fn event_codes(&self, event_type: u16) -> &[u8] {
        self.event_codes
            .get(usize::from(event_type))
            .map_or(&[], Vec::as_slice)
    }

    fn abs_info(&self, code: u16) -> Option<InputAbsInfo> {
        self.abs_info.get(usize::from(code)).copied().flatten()
    }

    fn receive_event(&self) -> Result<Option<RawInputEvent>, InputDeviceError> {
        let mut pending = self.pending.lock();
        if pending.is_empty() {
            self.fetch(&mut pending)?;
        }
        Ok(pending.pop_front())
    }

    fn finish_receive_batch(&self) -> Result<(), InputDeviceError> {
        // 驱动宿主在每次拉取后已结束本批。
        Ok(())
    }

    fn has_pending_event(&self) -> bool {
        if !self.pending.lock().is_empty() {
            return true;
        }
        call(&self.channel, &Writer::request(HAS_PENDING), Vec::new())
            .and_then(|reply| reply.reader().bool())
            .unwrap_or(false)
    }
}
