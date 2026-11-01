//! 以太网卡：帧随请求/回复内联传递；收发完成经活动 Event 通知使用者。

use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::Activity;
use device_model::network::{NetworkCompletion, NetworkDevice, NetworkError, NetworkStatistics};
use native_runtime::native_abi::{self, Status, rights};
use native_runtime::{Channel, Event};

use crate::directory::{self, Announcement, Class};
use crate::{ActivityBinding, EventHub, Reader, Writer, activity_signal, call, serve, take_handle};

const RECEIVE: u32 = 1;
const RESERVE: u32 = 2;
const SUBMIT: u32 = 3;
const CANCEL: u32 = 4;
const TRANSMIT_AVAILABLE: u32 = 5;
const POLL: u32 = 6;
const FINISH_RECEIVE: u32 = 7;
const STATISTICS: u32 = 8;
/// 接收缓冲：大于任何驱动接收槽能交付的帧（以太网 1514 字节帧加 VLAN 标签与余量）。
const MAX_FRAME: usize = 2048;

const ERROR_WOULD_BLOCK: Status = native_abi::ERR_SHOULD_WAIT;
const ERROR_TOO_LARGE: Status = native_abi::ERR_OUT_OF_RANGE;

fn encode_error(error: NetworkError) -> Status {
    match error {
        NetworkError::WouldBlock => ERROR_WOULD_BLOCK,
        NetworkError::FrameTooLarge => ERROR_TOO_LARGE,
        NetworkError::Device => native_abi::ERR_IO,
    }
}

fn decode_error(status: Status) -> NetworkError {
    match status {
        ERROR_WOULD_BLOCK => NetworkError::WouldBlock,
        ERROR_TOO_LARGE => NetworkError::FrameTooLarge,
        _ => NetworkError::Device,
    }
}

/// 驱动宿主：发布网卡。
///
/// # Errors
///
/// 对象、线程创建或目录写入失败。
pub fn publish(directory: &Channel, device: Arc<dyn NetworkDevice>) -> Result<(), Status> {
    let (client, server) = Channel::create()?;
    let event = Event::create()?;
    let peer_event = event.duplicate(rights::SAME)?;
    let mut description = directory::description();
    description
        .bytes(&device.mac_address())
        .bytes(device.driver().as_bytes());
    device.bind_activity(activity_signal(
        Arc::try_new(event).map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?);
    serve(
        server,
        2,
        b"net-server",
        Arc::try_new(
            move |ordinal: u32, request: &mut Reader<'_>, _, reply: &mut Writer| {
                match ordinal {
                    RECEIVE => {
                        let mut frame = [0u8; MAX_FRAME];
                        let length = device.receive(&mut frame).map_err(encode_error)?;
                        reply.bytes(&frame[..length]);
                    }
                    RESERVE => {
                        reply.u16(device.reserve_transmit().map_err(encode_error)?);
                    }
                    SUBMIT => {
                        let reservation = request.u16()?;
                        device
                            .submit_transmit(reservation, request.bytes()?)
                            .map_err(encode_error)?;
                    }
                    CANCEL => device.cancel_transmit(request.u16()?),
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
                    _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                }
                Ok(Vec::new())
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?;
    directory::announce(
        directory,
        Class::Network,
        &description,
        alloc::vec![client.into_handle(), peer_event.into_handle()],
    )
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
    mut announcement: Announcement,
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
