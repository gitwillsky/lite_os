#![no_std]
#![no_main]

extern crate user_lib;

use core::sync::atomic::{AtomicU32, Ordering};

use user_lib::{
    AT_REMOVEDIR, MAP_ANONYMOUS, MAP_FIXED_NOREPLACE, MAP_PRIVATE, O_CREAT, O_DIRECTORY, O_RDWR,
    O_TRUNC, PROT_EXEC, PROT_READ, PROT_WRITE, SigAction, clone_process, clone_thread, close,
    exit_group, exit_thread, fstat, fsync, ftruncate, futex_wait, futex_wake, getdents64, getpid,
    getppid, gettid, lseek, mkdirat, mmap, mprotect, munmap, openat, openat_from, read, renameat2,
    rt_sigaction, rt_sigprocmask, sched_yield, set_robust_list, tgkill, thread_pointer, unlinkat,
    unlinkat_from, wait4, write,
};

static SIGNAL_COUNT: AtomicU32 = AtomicU32::new(0);

extern "C" fn signal_handler(signal: usize, _info: usize, _context: usize) {
    if signal == 10 {
        SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

#[repr(C)]
struct ThreadProbe {
    start: AtomicU32,
    done: AtomicU32,
    child_tid: AtomicU32,
    parent_tid: AtomicU32,
    robust_head: [usize; 3],
    robust_node_next: usize,
    robust_futex: AtomicU32,
}

extern "C" fn thread_probe_main(argument: usize) -> ! {
    // SAFETY: parent keeps the ThreadProbe mapping live until clear-child-tid becomes zero.
    let probe = unsafe { &*(argument as *const ThreadProbe) };
    let wait = futex_wait(&probe.start as *const AtomicU32 as *const u32, 0);
    let ok = (wait == 0 || wait == -11)
        && probe.start.load(Ordering::Acquire) == 1
        && thread_pointer() == 0x1234_5000
        && set_robust_list(probe.robust_head.as_ptr() as *mut usize) == 0;
    probe.robust_futex.store(gettid() as u32, Ordering::Release);
    probe.done.store(if ok { 1 } else { 2 }, Ordering::Release);
    let _ = futex_wake(&probe.done as *const AtomicU32 as *const u32, 1);
    exit_thread(0)
}

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
    let probe_mapping = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS);
    let stack_mapping = mmap(
        0,
        4 * 4096,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
    );
    let thread_ok = if probe_mapping >= 0 && stack_mapping >= 0 {
        let probe_ptr = probe_mapping as *mut ThreadProbe;
        // SAFETY: probe mapping is a fresh aligned RW page owned by this Process.
        unsafe {
            probe_ptr.write(ThreadProbe {
                start: AtomicU32::new(0),
                done: AtomicU32::new(0),
                child_tid: AtomicU32::new(0),
                parent_tid: AtomicU32::new(0),
                robust_head: [0; 3],
                robust_node_next: 0,
                robust_futex: AtomicU32::new(0),
            });
            let head = &mut (*probe_ptr).robust_head as *mut [usize; 3] as usize;
            let node = &mut (*probe_ptr).robust_node_next as *mut usize as usize;
            (*probe_ptr).robust_head = [node, core::mem::size_of::<usize>(), 0];
            (*probe_ptr).robust_node_next = head;
        }
        // SAFETY: mappings remain live through clear-child-tid; entry never returns.
        let tid = unsafe {
            clone_thread(
                stack_mapping as usize + 4 * 4096,
                0x1234_5000,
                &mut (*probe_ptr).parent_tid as *mut AtomicU32 as *mut i32,
                &mut (*probe_ptr).child_tid as *mut AtomicU32 as *mut i32,
                thread_probe_main,
                probe_ptr as usize,
            )
        };
        if tid > 0 {
            // SAFETY: probe mapping remains shared by both Threads until child_tid is cleared.
            let probe = unsafe { &*probe_ptr };
            probe.start.store(1, Ordering::Release);
            let woke_child = futex_wake(&probe.start as *const AtomicU32 as *const u32, 1) >= 0;
            let waited_done = futex_wait(&probe.done as *const AtomicU32 as *const u32, 0);
            let done = probe.done.load(Ordering::Acquire) == 1;
            let observed_tid = probe.parent_tid.load(Ordering::Acquire) == tid as u32;
            let waited_exit = if probe.child_tid.load(Ordering::Acquire) == tid as u32 {
                futex_wait(
                    &probe.child_tid as *const AtomicU32 as *const u32,
                    tid as u32,
                )
            } else {
                -11
            };
            let cleared = probe.child_tid.load(Ordering::Acquire) == 0;
            let robust = probe.robust_futex.load(Ordering::Acquire) == 0x4000_0000;
            woke_child
                && (waited_done == 0 || waited_done == -11)
                && done
                && observed_tid
                && (waited_exit == 0 || waited_exit == -11)
                && cleared
                && robust
                && munmap(stack_mapping as usize, 4 * 4096) == 0
                && munmap(probe_mapping as usize, 4096) == 0
        } else {
            false
        }
    } else {
        false
    };
    let _ = write(
        1,
        if thread_ok {
            b"thread futex ok\n"
        } else {
            b"thread futex failed\n"
        },
    );
    let action = SigAction {
        handler: signal_handler as usize,
        flags: 4,
        mask: 0,
    };
    let signal_bit = 1u64 << 9;
    let signal_ok = rt_sigaction(10, Some(&action), None) == 0
        && rt_sigprocmask(0, Some(&signal_bit), None) == 0
        && tgkill(getpid() as usize, gettid() as usize, 10) == 0
        && SIGNAL_COUNT.load(Ordering::Relaxed) == 0
        && rt_sigprocmask(1, Some(&signal_bit), None) == 0
        && SIGNAL_COUNT.load(Ordering::Relaxed) == 1;
    let _ = write(
        1,
        if signal_ok {
            b"signal ok\n"
        } else {
            b"signal failed\n"
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
