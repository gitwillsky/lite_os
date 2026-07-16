use crate::ffi;

const COMPOSITOR_COMM: &[u8] = b"liteui-composit";
const SESSION_COMM: &[u8] = b"liteui-session";

#[derive(Clone, Copy)]
pub struct ProcessStat {
    comm: [u8; 15],
    comm_length: usize,
    parent: i32,
}

/// Authorizes only the root compositor directly spawned by init's session owner.
///
/// The Linux comm field is capped at 15 bytes, hence the explicit compositor
/// truncation. Checking the parent chain prevents an arbitrary root recovery
/// shell process from acquiring DRM/input authority through the broker.
pub fn is_controller(uid: u32, pid: i32) -> bool {
    if uid != 0 {
        return false;
    }
    let Some(compositor) = read_stat(pid) else {
        return false;
    };
    if compositor.name() != COMPOSITOR_COMM || compositor.parent <= 1 {
        return false;
    }
    let Some(session) = read_stat(compositor.parent) else {
        return false;
    };
    session.name() == SESSION_COMM && session.parent == 1
}

fn read_stat(pid: i32) -> Option<ProcessStat> {
    if pid <= 0 {
        return None;
    }
    let mut path = [0u8; 32];
    let prefix = b"/proc/";
    path[..prefix.len()].copy_from_slice(prefix);
    let mut length = prefix.len();
    length += decimal(pid as u32, &mut path[length..]);
    path[length..length + 5].copy_from_slice(b"/stat");
    length += 5;
    path[length] = 0;
    let fd = unsafe { ffi::open(path.as_ptr().cast(), ffi::O_RDONLY | ffi::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let mut bytes = [0u8; 512];
    let count = loop {
        let count = unsafe { ffi::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        }
        break count;
    };
    unsafe { ffi::close(fd) };
    (count > 0)
        .then(|| parse(&bytes[..count as usize]))
        .flatten()
}

fn parse(bytes: &[u8]) -> Option<ProcessStat> {
    let open = bytes.iter().position(|byte| *byte == b'(')?;
    let close = bytes.iter().rposition(|byte| *byte == b')')?;
    let name = bytes.get(open + 1..close)?;
    if name.len() > 15 || bytes.get(close + 1) != Some(&b' ') {
        return None;
    }
    let mut fields = bytes[close + 2..].split(u8::is_ascii_whitespace);
    fields.next()?;
    let parent = number(fields.next()?)?;
    let mut comm = [0u8; 15];
    comm[..name.len()].copy_from_slice(name);
    Some(ProcessStat {
        comm,
        comm_length: name.len(),
        parent,
    })
}

impl ProcessStat {
    fn name(&self) -> &[u8] {
        &self.comm[..self.comm_length]
    }
}

fn number(bytes: &[u8]) -> Option<i32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    bytes.iter().try_fold(0i32, |value, byte| {
        value.checked_mul(10)?.checked_add(i32::from(byte - b'0'))
    })
}

fn decimal(mut value: u32, output: &mut [u8]) -> usize {
    let mut reversed = [0u8; 10];
    let mut length = 0;
    loop {
        reversed[length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for index in 0..length {
        output[index] = reversed[length - index - 1];
    }
    length
}
