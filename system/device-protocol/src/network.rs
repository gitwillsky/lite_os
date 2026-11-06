//! 以太网卡：帧随请求/回复内联传递；收发完成经活动 Event 通知使用者。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::Activity;
use device_model::network::{
    NetworkCompletion, NetworkDevice, NetworkError, NetworkLink, NetworkStatistics,
};
use native_runtime::native_abi::{self, Status, rights};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event};

use crate::directory::{self, Class, Directory, OpenedDevice, Session, SessionState};
use crate::{ActivityBinding, EventHub, Reader, Writer, activity_signal, call, take_handle};

const RECEIVE: u32 = 1;
const RESERVE: u32 = 2;
const SUBMIT: u32 = 3;
const CANCEL: u32 = 4;
const TRANSMIT_AVAILABLE: u32 = 5;
const POLL: u32 = 6;
const FINISH_RECEIVE: u32 = 7;
const STATISTICS: u32 = 8;
const LINK: u32 = 9;
/// 接收缓冲：大于任何驱动接收槽能交付的帧（以太网 1514 字节帧加 VLAN 标签与余量）。
const MAX_FRAME: usize = 2048;

const ERROR_WOULD_BLOCK: Status = native_abi::ERR_SHOULD_WAIT;
const ERROR_TOO_LARGE: Status = native_abi::ERR_OUT_OF_RANGE;

fn encode_error(error: NetworkError) -> Status {
    match error {
        NetworkError::WouldBlock => ERROR_WOULD_BLOCK,
        NetworkError::FrameTooLarge => ERROR_TOO_LARGE,
        NetworkError::Device => native_abi::ERR_IO,
        NetworkError::Disconnected => native_abi::ERR_PEER_CLOSED,
    }
}

fn decode_error(status: Status) -> NetworkError {
    match status {
        ERROR_WOULD_BLOCK => NetworkError::WouldBlock,
        ERROR_TOO_LARGE => NetworkError::FrameTooLarge,
        // 会话 channel 关闭：call 返回 PEER_CLOSED，与设备内部错误区分，使用者据此移除接口。
        native_abi::ERR_PEER_CLOSED => NetworkError::Disconnected,
        _ => NetworkError::Device,
    }
}

/// 一个会话的服务端状态：会话结束时取消仍未提交的发送预留，使用者崩溃不会让发送槽永久泄漏。
/// 只接受本会话发出的预留，防止客户端重复取消使驱动 fail-stop。
struct NetworkSession {
    device: Arc<dyn NetworkDevice>,
    // OWNER: 本会话已预留、尚未提交或取消的发送槽；缺失时崩溃的使用者会永久占住发送池。
    reservations: Mutex<Vec<u16>>,
}

impl NetworkSession {
    fn release(&self, reservation: u16) -> Result<(), Status> {
        let mut reservations = self.reservations.lock();
        let index = reservations
            .iter()
            .position(|held| *held == reservation)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        reservations.swap_remove(index);
        Ok(())
    }

    fn handle(
        &self,
        ordinal: u32,
        request: &mut Reader<'_>,
        reply: &mut Writer,
    ) -> Result<(), Status> {
        let device = &self.device;
        match ordinal {
            RECEIVE => {
                let mut frame = [0u8; MAX_FRAME];
                let length = device.receive(&mut frame).map_err(encode_error)?;
                reply.bytes(&frame[..length]);
            }
            RESERVE => {
                let mut reservations = self.reservations.lock();
                reservations
                    .try_reserve(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                let reservation = device.reserve_transmit().map_err(encode_error)?;
                reservations.push(reservation);
                reply.u16(reservation);
            }
            SUBMIT => {
                let reservation = request.u16()?;
                let frame = request.bytes()?;
                if !self.reservations.lock().contains(&reservation) {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                // 提交失败时预留仍归本会话，由使用者取消或随会话结束取消。
                device
                    .submit_transmit(reservation, frame)
                    .map_err(encode_error)?;
                self.release(reservation)?;
            }
            CANCEL => {
                let reservation = request.u16()?;
                self.release(reservation)?;
                device.cancel_transmit(reservation);
            }
            TRANSMIT_AVAILABLE => {
                reply.bool(device.transmit_available());
            }
            POLL => {
                let completion = device
                    .poll_completions(request.usize()?)
                    .map_err(encode_error)?;
                reply
                    .bool(completion.backlog)
                    .bool(completion.transmit_became_available);
            }
            FINISH_RECEIVE => device.finish_receive_batch().map_err(encode_error)?,
            STATISTICS => {
                let statistics = device.statistics();
                for value in [
                    statistics.received_bytes,
                    statistics.received_packets,
                    statistics.received_errors,
                    statistics.received_dropped,
                    statistics.transmitted_bytes,
                    statistics.transmitted_packets,
                    statistics.transmitted_errors,
                    statistics.transmitted_dropped,
                ] {
                    reply.u64(value);
                }
            }
            LINK => {
                let link = device.link().map_err(encode_error)?;
                reply
                    .bool(link.carrier)
                    .u32(link.speed_mbps.unwrap_or(u32::MAX));
            }
            _ => return Err(native_abi::ERR_NOT_SUPPORTED),
        }
        Ok(())
    }
}

impl Drop for NetworkSession {
    fn drop(&mut self) {
        for reservation in self.reservations.get_mut().drain(..) {
            self.device.cancel_transmit(reservation);
        }
    }
}

/// 驱动宿主：登记网卡（独占：同一时刻只有一个协议栈驱动它，协议栈重启后重新打开）。
///
/// # Errors
///
/// 对象创建或分配失败。
pub fn publish(directory: &Directory, device: Arc<dyn NetworkDevice>) -> Result<(), Status> {
    let event = Arc::try_new(Event::create()?).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let mut description = directory::description();
    description
        .bytes(&device.mac_address())
        .bytes(device.driver().as_bytes());
    device.bind_activity(activity_signal(event.clone())?);
    directory
        .add(
            Class::Network,
            &description,
            true,
            Box::new(move |end| {
                let session = SessionState::new(
                    NetworkSession {
                        device: device.clone(),
                        reservations: Mutex::new(Vec::new()),
                    },
                    end,
                );
                let (client, server) = Channel::create()?;
                let server = directory::serve_session(
                    server,
                    2,
                    b"net-server",
                    Arc::try_new(
                        move |ordinal: u32, request: &mut Reader<'_>, _, reply: &mut Writer| {
                            session
                                .get()
                                .handle(ordinal, request, reply)
                                .map(|()| Vec::new())
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

/// 使用者端的远端网卡。
pub struct RemoteNetwork {
    channel: Channel,
    activity: ActivityBinding,
    mac: [u8; 6],
    driver: &'static str,
}

/// 连接目录中的网卡。
///
/// 驱动名在设备存续期间不变，按 trait 约定以 `'static` 返回：连接时泄漏一份副本（每块网卡一次）。
///
/// # Errors
///
/// 描述或句柄不完整，或事件登记失败。
pub fn connect(
    mut announcement: OpenedDevice,
    hub: &Arc<EventHub>,
) -> Result<Arc<RemoteNetwork>, Status> {
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    let activity = hub.activity(Event::from_handle(take_handle(
        &mut announcement.handles,
        1,
    )?))?;
    let mut reader = announcement.reader();
    let mac: [u8; 6] = reader
        .bytes()?
        .try_into()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let driver = core::str::from_utf8(reader.bytes()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let driver: &'static str = alloc::boxed::Box::leak(alloc::boxed::Box::<str>::from(driver));
    // 会话断开时活动 Event 不再置位：关闭通知让使用者再推进一轮，从调用错误观察到断开。
    hub.on_closed(channel.duplicate(rights::SAME)?, &activity)?;
    Arc::try_new(RemoteNetwork {
        channel,
        activity,
        mac,
        driver,
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}

impl NetworkDevice for RemoteNetwork {
    fn bind_activity(&self, activity: Activity) {
        self.activity.bind(activity);
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn driver(&self) -> &'static str {
        self.driver
    }

    fn link(&self) -> Result<NetworkLink, NetworkError> {
        let reply =
            call(&self.channel, &Writer::request(LINK), Vec::new()).map_err(decode_error)?;
        let mut reader = reply.reader();
        let carrier = reader.bool().map_err(|_| NetworkError::Device)?;
        let speed = reader.u32().map_err(|_| NetworkError::Device)?;
        Ok(NetworkLink {
            carrier,
            speed_mbps: (speed <= i32::MAX as u32).then_some(speed),
        })
    }

    fn receive(&self, frame: &mut [u8]) -> Result<usize, NetworkError> {
        let reply =
            call(&self.channel, &Writer::request(RECEIVE), Vec::new()).map_err(decode_error)?;
        let data = reply.reader().bytes().map_err(|_| NetworkError::Device)?;
        let target = frame
            .get_mut(..data.len())
            .ok_or(NetworkError::FrameTooLarge)?;
        target.copy_from_slice(data);
        Ok(data.len())
    }

    fn reserve_transmit(&self) -> Result<u16, NetworkError> {
        let reply =
            call(&self.channel, &Writer::request(RESERVE), Vec::new()).map_err(decode_error)?;
        reply.reader().u16().map_err(|_| NetworkError::Device)
    }

    fn submit_transmit(&self, reservation: u16, frame: &[u8]) -> Result<(), NetworkError> {
        let mut request = Writer::request(SUBMIT);
        request.u16(reservation).bytes(frame);
        call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        Ok(())
    }

    fn cancel_transmit(&self, reservation: u16) {
        let mut request = Writer::request(CANCEL);
        request.u16(reservation);
        let _ = call(&self.channel, &request, Vec::new());
    }

    fn transmit_available(&self) -> bool {
        call(
            &self.channel,
            &Writer::request(TRANSMIT_AVAILABLE),
            Vec::new(),
        )
        .and_then(|reply| reply.reader().bool())
        .unwrap_or(false)
    }

    fn poll_completions(&self, budget: usize) -> Result<NetworkCompletion, NetworkError> {
        let mut request = Writer::request(POLL);
        request.u64(budget as u64);
        let reply = call(&self.channel, &request, Vec::new()).map_err(decode_error)?;
        let mut reader = reply.reader();
        Ok(NetworkCompletion {
            backlog: reader.bool().map_err(|_| NetworkError::Device)?,
            transmit_became_available: reader.bool().map_err(|_| NetworkError::Device)?,
        })
    }

    fn finish_receive_batch(&self) -> Result<(), NetworkError> {
        call(&self.channel, &Writer::request(FINISH_RECEIVE), Vec::new()).map_err(decode_error)?;
        Ok(())
    }

    fn statistics(&self) -> NetworkStatistics {
        let Ok(reply) = call(&self.channel, &Writer::request(STATISTICS), Vec::new()) else {
            return NetworkStatistics::default();
        };
        let mut reader = reply.reader();
        let mut next = || reader.u64().unwrap_or(0);
        NetworkStatistics {
            received_bytes: next(),
            received_packets: next(),
            received_errors: next(),
            received_dropped: next(),
            transmitted_bytes: next(),
            transmitted_packets: next(),
            transmitted_errors: next(),
            transmitted_dropped: next(),
        }
    }
}
