//! channel、event、port 调用。

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_abi::{ChannelCallArgs, PortPacket, Status, rights};

use super::Call;
use crate::object::{Channel, Event, HandleEntry, Message, Object, Port, ReadError};

pub(super) fn channel_create(call: &Call<'_>) -> Result<(), Status> {
    let (first, second) = Channel::create_pair()?;
    let first = Arc::try_new(first).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let second = Arc::try_new(second).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let handle = call.publish(Object::Channel(first), call.arg(0))?;
    if let Err(status) = call.publish(Object::Channel(second), call.arg(1)) {
        drop(call.process().handles().remove(handle));
        return Err(status);
    }
    Ok(())
}

/// 从调用者句柄表移出要随消息转移的句柄；任一失败时全部保留在调用者表中并返回错误。
fn take_message_handles(
    call: &Call<'_>,
    channel: &Arc<Channel>,
    address: usize,
    count: usize,
    entries: &mut Vec<HandleEntry>,
) -> Result<(), Status> {
    if count > native_abi::CHANNEL_MAX_HANDLES {
        return Err(native_abi::ERR_OUT_OF_RANGE);
    }
    // 标准 ABI 最多 64 个值，固定栈存储避免仅为句柄编号再分配一个 Vec。
    let mut values = [native_abi::HANDLE_INVALID; native_abi::CHANNEL_MAX_HANDLES];
    let handles = &mut values[..count];
    call.read_array_into(address, handles)?;
    let table = call.process().handles();
    for &handle in handles.iter() {
        let entry = table.get(handle)?;
        if entry.rights & rights::TRANSFER == 0 {
            return Err(native_abi::ERR_ACCESS_DENIED);
        }
        // 写入自身端点会让端点经自身队列保活，形成永不释放的环。
        if let Object::Channel(candidate) = &entry.object
            && Arc::ptr_eq(candidate, channel)
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
    }
    table.remove_all(handles, entries)?;
    Ok(())
}

fn build_message(
    call: &Call<'_>,
    channel: &Arc<Channel>,
    bytes: usize,
    length: usize,
    handles: usize,
    count: usize,
) -> Result<Message, Status> {
    let mut message = channel.prepare_message(length, count)?;
    call.space()?
        .copy_from_user(bytes, message.bytes_mut())
        .map_err(|_| native_abi::ERR_INVALID_POINTER)?;
    take_message_handles(call, channel, handles, count, message.handles_mut())?;
    Ok(message)
}

pub(super) fn channel_write(call: &Call<'_>) -> Result<(), Status> {
    let channel = call.channel(call.arg_handle(0), rights::WRITE)?;
    let message = build_message(
        call,
        &channel,
        call.arg(1),
        call.arg(2),
        call.arg(3),
        call.arg(4),
    )?;
    // 失败时消息（含已移出的句柄）在这里释放，与 Zircon“句柄总是被消费”的语义一致。
    channel.write(message).map_err(|(status, message)| {
        drop(message);
        status
    })
}

/// 把消息句柄放进调用者表并写出；失败时句柄随消息一起释放。
fn deliver(
    call: &Call<'_>,
    mut message: Message,
    bytes: usize,
    handles: usize,
    actual: usize,
) -> Result<(), Status> {
    let counts = message.actual();
    let mut values = [native_abi::HANDLE_INVALID; native_abi::CHANNEL_MAX_HANDLES];
    let values = &mut values[..counts.handles as usize];
    call.process()
        .handles()
        .insert_all(message.handles_mut(), values)?;
    let written = call
        .write_bytes(bytes, message.bytes())
        .and_then(|()| call.write_array(handles, values))
        .and_then(|()| call.write_optional(actual, &counts));
    if written.is_err() {
        for &handle in values.iter() {
            drop(call.process().handles().remove(handle));
        }
    }
    written
}

pub(super) fn channel_read(call: &Call<'_>) -> Result<(), Status> {
    let channel = call.channel(call.arg_handle(0), rights::READ)?;
    match channel.read(call.arg(2), call.arg(4)) {
        Ok(message) => deliver(call, message, call.arg(1), call.arg(3), call.arg(5)),
        Err(ReadError { status, actual }) => {
            if status == native_abi::ERR_BUFFER_TOO_SMALL {
                call.write_optional(call.arg(5), &actual)?;
            }
            Err(status)
        }
    }
}

pub(super) fn channel_call(call: &Call<'_>) -> Result<(), Status> {
    let channel = call.channel(call.arg_handle(0), rights::READ | rights::WRITE)?;
    let args: ChannelCallArgs = call.read(call.arg(1))?;
    let message = build_message(
        call,
        &channel,
        args.write_bytes as usize,
        args.write_num_bytes as usize,
        args.write_handles as usize,
        args.write_num_handles as usize,
    )?;
    let reply = call
        .owner
        .channel_call(&channel, message, super::deadline(call.arg(2)))
        .map_err(|(status, message)| {
            drop(message);
            status
        })?;
    let actual = reply.actual();
    if actual.bytes > args.read_num_bytes || actual.handles > args.read_num_handles {
        call.write_optional(call.arg(3), &actual)?;
        return Err(native_abi::ERR_BUFFER_TOO_SMALL);
    }
    deliver(
        call,
        reply,
        args.read_bytes as usize,
        args.read_handles as usize,
        call.arg(3),
    )
}

pub(super) fn event_create(call: &Call<'_>) -> Result<(), Status> {
    let event = Arc::try_new(Event::new()).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    call.publish(Object::Event(event), call.arg(0)).map(|_| ())
}

pub(super) fn port_create(call: &Call<'_>) -> Result<(), Status> {
    let port = Arc::try_new(Port::new()).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    call.publish(Object::Port(port), call.arg(0)).map(|_| ())
}

pub(super) fn port_wait(call: &Call<'_>) -> Result<(), Status> {
    let handle = call.arg_handle(0);
    let port = call.port(handle, rights::READ)?;
    let key = call.process().handles().cancel_key(handle);
    port.wait(key, super::deadline(call.arg(1)), |packet| {
        call.write(call.arg(2), packet)
    })
}

pub(super) fn port_queue(call: &Call<'_>) -> Result<(), Status> {
    let port = call.port(call.arg_handle(0), rights::WRITE)?;
    port.queue(PortPacket {
        key: call.arg(1) as u64,
        kind: native_abi::packet::USER,
        status: native_abi::OK,
        trigger: 0,
        observed: 0,
        payload: call.arg(2) as u64,
    })
}
