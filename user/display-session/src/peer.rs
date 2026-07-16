use crate::ffi;

#[derive(Clone, Copy)]
pub struct ProcessStat {
    pub comm: [u8; 15],
    pub comm_length: usize,
    pub parent: i32,
    pub group: i32,
    pub tty: i32,
    pub terminal_group: i32,
}

pub fn read_stat(pid: i32) -> Option<ProcessStat> {
    if pid <= 0 {
        return None;
    }
    let mut path = [0u8; 32];
    let prefix = b"/proc/";
    path[..prefix.len()].copy_from_slice(prefix);
    let mut length = prefix.len();
    length += decimal(pid as u32, &mut path[length..]);
    let suffix = b"/stat";
    path[length..length + suffix.len()].copy_from_slice(suffix);
    length += suffix.len();
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

pub fn is_terminal(uid: u32, stat: ProcessStat) -> bool {
    uid == 0 && stat.parent == 1 && &stat.comm[..stat.comm_length] == b"liteos-terminal"
}

pub fn is_foreground_descendant(pid: i32, terminal_pid: i32) -> bool {
    let Some(peer) = read_stat(pid) else {
        return false;
    };
    if peer.tty == 0 || peer.group <= 0 || peer.group != peer.terminal_group {
        return false;
    }
    let mut current = pid;
    for _ in 0..64 {
        if current == terminal_pid {
            return true;
        }
        let Some(stat) = read_stat(current) else {
            return false;
        };
        if stat.parent <= 1 || stat.parent == current {
            return stat.parent == terminal_pid;
        }
        current = stat.parent;
    }
    false
}

fn parse(bytes: &[u8]) -> Option<ProcessStat> {
    let open = bytes.iter().position(|byte| *byte == b'(')?;
    let close = bytes.iter().rposition(|byte| *byte == b')')?;
    if close <= open || bytes.get(close + 1) != Some(&b' ') {
        return None;
    }
    let name = bytes.get(open + 1..close)?;
    if name.len() > 15 {
        return None;
    }
    let mut comm = [0u8; 15];
    comm[..name.len()].copy_from_slice(name);
    let mut fields = bytes[close + 2..].split(|byte| byte.is_ascii_whitespace());
    fields.next()?;
    Some(ProcessStat {
        comm,
        comm_length: name.len(),
        parent: number(fields.next()?)?,
        group: number(fields.next()?)?,
        tty: {
            fields.next()?;
            number(fields.next()?)?
        },
        terminal_group: number(fields.next()?)?,
    })
}

fn number(bytes: &[u8]) -> Option<i32> {
    let (negative, digits) = if bytes.first() == Some(&b'-') {
        (true, &bytes[1..])
    } else {
        (false, bytes)
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value = digits.iter().try_fold(0i32, |value, byte| {
        value.checked_mul(10)?.checked_add(i32::from(byte - b'0'))
    })?;
    if negative {
        value.checked_neg()
    } else {
        Some(value)
    }
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
