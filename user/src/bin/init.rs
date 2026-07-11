#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{
    AT_REMOVEDIR, O_CREAT, O_DIRECTORY, O_RDWR, O_TRUNC, close, fstat, fsync, ftruncate,
    getdents64, lseek, mkdirat, openat, openat_from, read, renameat2, sched_yield, unlinkat,
    unlinkat_from, write,
};

fn directory_contains(buffer: &[u8], name: &[u8]) -> bool {
    let mut offset = 0;
    while offset + 19 <= buffer.len() {
        let record = u16::from_ne_bytes([buffer[offset + 16], buffer[offset + 17]]) as usize;
        if record < 20 || offset + record > buffer.len() {
            return false;
        }
        let bytes = &buffer[offset + 19..offset + record];
        let length = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        if &bytes[..length] == name {
            return true;
        }
        offset += record;
    }
    false
}

#[unsafe(no_mangle)]
extern "C" fn main(_argc: usize, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let _ = write(1, b"LiteOS init\n");
    let payload = b"ext2 read-write persistence\n";
    let previous = openat(b"/rw-check\0", O_RDWR, 0);
    if previous >= 0 {
        let mut old = [0u8; 28];
        if read(previous as usize, &mut old) == payload.len() as isize && old == *payload {
            let _ = write(1, b"ext2 persisted\n");
        }
        let _ = close(previous as usize);
    }
    let _ = unlinkat(b"/rw-check.tmp\0");
    let fd = openat(b"/rw-check.tmp\0", O_CREAT | O_TRUNC | O_RDWR, 0o644);
    if fd >= 0 {
        let mut result = [0u8; 28];
        let mut stat = [0u8; 128];
        let ok = write(fd as usize, payload) == payload.len() as isize
            && ftruncate(fd as usize, 4096) == 0
            && ftruncate(fd as usize, payload.len()) == 0
            && fstat(fd as usize, &mut stat) == 0
            && u64::from_ne_bytes(stat[48..56].try_into().unwrap()) == payload.len() as u64
            && fsync(fd as usize) == 0
            && lseek(fd as usize, 0, 0) == 0
            && read(fd as usize, &mut result) == payload.len() as isize
            && result == *payload
            && close(fd as usize) == 0
            && renameat2(b"/rw-check.tmp\0", b"/rw-check\0") == 0;
        let directory = openat(b"/\0", O_DIRECTORY, 0);
        let mut entries = [0u8; 512];
        let listed = directory >= 0
            && getdents64(directory as usize, &mut entries) > 0
            && directory_contains(&entries, b"rw-check")
            && close(directory as usize) == 0;
        let deleted = openat(b"/rw-delete\0", O_CREAT | O_TRUNC | O_RDWR, 0o600);
        let reclaimed = deleted >= 0
            && write(deleted as usize, b"reclaim") == 7
            && close(deleted as usize) == 0
            && unlinkat(b"/rw-delete\0") == 0;
        let held = openat(b"/rw-held\0", O_CREAT | O_TRUNC | O_RDWR, 0o600);
        let mut held_data = [0u8; 4];
        let deferred = held >= 0
            && write(held as usize, b"held") == 4
            && unlinkat(b"/rw-held\0") == 0
            && lseek(held as usize, 0, 0) == 0
            && read(held as usize, &mut held_data) == 4
            && held_data == *b"held"
            && close(held as usize) == 0
            && openat(b"/rw-held\0", O_RDWR, 0) < 0;
        let _ = unlinkat(b"/rw-dir/item\0");
        let _ = unlinkat_from(user_lib::AT_FDCWD, b"/rw-dir\0", AT_REMOVEDIR);
        let directory_fd = if mkdirat(b"/rw-dir\0", 0o755) == 0 {
            openat(b"/rw-dir\0", O_DIRECTORY, 0)
        } else {
            -1
        };
        let relative = if directory_fd >= 0 {
            openat_from(directory_fd, b"item\0", O_CREAT | O_RDWR, 0o644)
        } else {
            -1
        };
        let directory_mutation = relative >= 0
            && write(relative as usize, b"item") == 4
            && close(relative as usize) == 0
            && unlinkat_from(directory_fd, b"item\0", 0) == 0
            && close(directory_fd as usize) == 0
            && unlinkat_from(user_lib::AT_FDCWD, b"/rw-dir\0", AT_REMOVEDIR) == 0;
        let _ = write(
            1,
            if ok && listed && reclaimed && deferred && directory_mutation {
                b"ext2 rw ok\n"
            } else {
                b"ext2 rw failed\n"
            },
        );
    }
    loop {
        let _ = sched_yield();
    }
}
