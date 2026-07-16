pub const CLIENT_OPEN_SEAT: u16 = 1;
pub const CLIENT_CLOSE_SEAT: u16 = 2;
pub const CLIENT_OPEN_DEVICE: u16 = 3;
pub const CLIENT_CLOSE_DEVICE: u16 = 4;
pub const CLIENT_DISABLE_SEAT: u16 = 5;
pub const CLIENT_SWITCH_SESSION: u16 = 6;
pub const CLIENT_PING: u16 = 7;
pub const SERVER_SEAT_OPENED: u16 = 0x8001;
pub const SERVER_SEAT_CLOSED: u16 = 0x8002;
pub const SERVER_DEVICE_OPENED: u16 = 0x8003;
pub const SERVER_DEVICE_CLOSED: u16 = 0x8004;
pub const SERVER_DISABLE_SEAT: u16 = 0x8005;
pub const SERVER_ENABLE_SEAT: u16 = 0x8006;
pub const SERVER_PONG: u16 = 0x8007;
pub const SERVER_SEAT_DISABLED: u16 = 0x8009;
pub const SERVER_ERROR: u16 = 0xffff;
pub const MAX_PATH: usize = 256;

#[derive(Clone, Copy)]
pub enum Request {
    OpenSeat,
    CloseSeat,
    OpenDevice { path: [u8; MAX_PATH], length: usize },
    CloseDevice(i32),
    DisableSeat,
    SwitchSession,
    Ping,
}

pub enum Decode {
    Incomplete,
    Invalid,
    Complete { request: Request, consumed: usize },
}

pub fn decode(bytes: &[u8]) -> Decode {
    if bytes.len() < 4 {
        return Decode::Incomplete;
    }
    let opcode = u16::from_ne_bytes([bytes[0], bytes[1]]);
    let size = usize::from(u16::from_ne_bytes([bytes[2], bytes[3]]));
    let Some(total) = 4usize.checked_add(size) else {
        return Decode::Invalid;
    };
    if total > 4 + 2 + MAX_PATH {
        return Decode::Invalid;
    }
    if bytes.len() < total {
        return Decode::Incomplete;
    }
    let payload = &bytes[4..total];
    let request = match opcode {
        CLIENT_OPEN_SEAT if payload.is_empty() => Request::OpenSeat,
        CLIENT_CLOSE_SEAT if payload.is_empty() => Request::CloseSeat,
        CLIENT_OPEN_DEVICE => match decode_path(payload) {
            Some((path, length)) => Request::OpenDevice { path, length },
            None => return Decode::Invalid,
        },
        CLIENT_CLOSE_DEVICE if payload.len() == 4 => Request::CloseDevice(i32::from_ne_bytes([
            payload[0], payload[1], payload[2], payload[3],
        ])),
        CLIENT_DISABLE_SEAT if payload.is_empty() => Request::DisableSeat,
        CLIENT_SWITCH_SESSION if payload.len() == 4 => Request::SwitchSession,
        CLIENT_PING if payload.is_empty() => Request::Ping,
        _ => return Decode::Invalid,
    };
    Decode::Complete {
        request,
        consumed: total,
    }
}

fn decode_path(payload: &[u8]) -> Option<([u8; MAX_PATH], usize)> {
    if payload.len() < 3 {
        return None;
    }
    let length = usize::from(u16::from_ne_bytes([payload[0], payload[1]]));
    let source = payload.get(2..)?;
    if source.len() != length
        || length == 0
        || length > MAX_PATH
        || source[length - 1] != 0
        || source[..length - 1].contains(&0)
    {
        return None;
    }
    let mut path = [0u8; MAX_PATH];
    path[..length].copy_from_slice(source);
    Some((path, length))
}

pub fn frame(opcode: u16, payload: &[u8], output: &mut [u8; 80]) -> Option<usize> {
    let size = u16::try_from(payload.len()).ok()?;
    let length = 4usize.checked_add(payload.len())?;
    if length > output.len() {
        return None;
    }
    output[..2].copy_from_slice(&opcode.to_ne_bytes());
    output[2..4].copy_from_slice(&size.to_ne_bytes());
    output[4..length].copy_from_slice(payload);
    Some(length)
}
