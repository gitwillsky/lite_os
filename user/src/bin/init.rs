#![no_std]
#![no_main]

extern crate user_lib;

use user_lib::{
    AT_REMOVEDIR, MAP_ANONYMOUS, MAP_FIXED_NOREPLACE, MAP_PRIVATE, O_CREAT, O_DIRECTORY, O_RDWR,
    O_TRUNC, PROT_EXEC, PROT_READ, PROT_WRITE, clone_process, close, exit_group, fstat, fsync,
    ftruncate, getdents64, getppid, lseek, mkdirat, mmap, mprotect, munmap, openat, openat_from,
    read, renameat2, sched_yield, unlinkat, unlinkat_from, wait4, write,
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
    let mapping = mmap(
        0,
        3 * 4096,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
    );
    let vma_ok = if mapping >= 0 {
        let base = mapping as usize;
        // SAFETY: mmap 成功后这三页属于当前进程且为 RW；每次访问均位于映射页内。
        unsafe {
            (base as *mut u8).write_volatile(0x11);
            ((base + 4096) as *mut u8).write_volatile(0x22);
            ((base + 8192) as *mut u8).write_volatile(0x33);
        }
        let protected = mprotect(base + 4096, 4096, PROT_READ) == 0
            && mprotect(base + 4096, 4096, PROT_READ | PROT_WRITE) == 0;
        // SAFETY: 第二次 mprotect 已恢复中间页 RW，指针仍位于原映射。
        unsafe { ((base + 4096) as *mut u8).write_volatile(0x44) };
        let split = munmap(base + 4096, 4096) == 0;
        let remapped = mmap(
            base + 4096,
            4096,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
        ) == (base + 4096) as isize;
        let collision = mmap(
            base + 4096,
            4096,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
        ) == -17;
        let wx_rejected = mmap(0, 4096, PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS) == -13;
        // SAFETY: first/last values remain mapped; middle was remapped and is zero-filled.
        let contents = unsafe {
            (base as *const u8).read_volatile() == 0x11
                && ((base + 4096) as *const u8).read_volatile() == 0
                && ((base + 8192) as *const u8).read_volatile() == 0x33
        };
        protected
            && split
            && remapped
            && collision
            && wx_rejected
            && contents
            && munmap(base, 3 * 4096) == 0
    } else {
        false
    };
    let _ = write(1, if vma_ok { b"vma ok\n" } else { b"vma failed\n" });
    let mut fork_probe = 0x51u8;
    let child = clone_process();
    if child == 0 {
        fork_probe = 0x52;
        exit_group(if getppid() == 1 && fork_probe == 0x52 {
            23
        } else {
            24
        });
    }
    let mut child_status = 0i32;
    let process_ok = child > 0
        && wait4(child, Some(&mut child_status), 0) == child
        && child_status == 23 << 8
        && fork_probe == 0x51
        && wait4(child, None, 0) == -10;
    let _ = write(
        1,
        if process_ok {
            b"process ok\n"
        } else {
            b"process failed\n"
        },
    );
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
