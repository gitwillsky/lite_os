//! PTY reads and boot-log replay; PTY ownership lives in `linux-uapi`.

use std::{
    fs::OpenOptions,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
};

use linux_uapi::pty::PtySession;

use crate::{
    input::{InputQueue, PTY_REPLY_EXPANSION},
    model::Model,
};

const PTY_BUDGET: usize = 64 * 1024;
const O_NONBLOCK: i32 = 0x800;

pub fn read_pty(
    session: &mut PtySession,
    model: &mut Model,
    input: &mut InputQueue,
) -> (bool, bool) {
    let mut total = 0;
    let mut changed = false;
    let mut bytes = [0u8; 8 * 1024];
    while total < PTY_BUDGET {
        let capacity = bytes
            .len()
            .min(PTY_BUDGET - total)
            .min(input.remaining() / PTY_REPLY_EXPANSION);
        if capacity == 0 {
            return (changed, false);
        }
        match session.read(&mut bytes[..capacity]) {
            Ok(0) => return (changed, true),
            Ok(count) => {
                model.feed(&bytes[..count], |reply| input.push(reply));
                total += count;
                changed = true;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return (changed, false),
            Err(_) => return (changed, true),
        }
    }
    (changed, false)
}

pub fn replay_boot_log(model: &mut Model) {
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open("/dev/kmsg")
    else {
        return;
    };
    let mut record = [0u8; 256];
    loop {
        match file.read(&mut record) {
            Ok(0) => break,
            Ok(count) => {
                let bytes = &record[..count];
                if let Some(separator) = bytes.iter().position(|byte| *byte == b';') {
                    model.feed(&bytes[separator + 1..], |_| {});
                    if bytes.last() != Some(&b'\n') {
                        model.feed(b"\n", |_| {});
                    }
                }
            }
            Err(error) if error.raw_os_error() == Some(32) => continue,
            Err(_) => break,
        }
    }
}
