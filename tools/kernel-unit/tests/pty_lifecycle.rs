#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际 PTY pair、Terminal、Pipe 与索引 owner；job-control callback 是可计数的 task seam。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/storage/fs/device/number.rs"]
mod device_number;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../../kernel/src/id.rs"]
mod id;
#[path = "../src/pipe_fixture.rs"]
mod ipc;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../src/sync.rs"]
mod sync;
mod fs {
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum FileSystemError {
        IoError,
        AlreadyExists,
        InvalidOperation,
        OutOfMemory,
        NoSpace,
        NotFound,
    }
    pub(crate) mod device {
        pub(crate) use crate::device_number::DeviceNumber;
    }
}
#[path = "../../../kernel/src/devices/tty"]
mod tty {
    pub(crate) mod pty;
    #[allow(
        unused_imports,
        reason = "focused TTY fixture consumes a subset of the terminal facade"
    )]
    pub(crate) mod terminal;
    pub(crate) const PTS_MAJOR: u32 = 136;
    pub(crate) const PTS_MINOR_COUNT: u32 = 65536;
    pub(crate) static HANGUPS: core::sync::atomic::AtomicUsize =
        core::sync::atomic::AtomicUsize::new(0);
    pub(crate) fn hangup(terminal: &terminal::Terminal) {
        HANGUPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        drop(terminal.hangup());
    }
    pub(crate) fn deliver_input_signals(_: &terminal::Terminal, _: u64) {}
    pub(crate) fn master() -> Result<alloc::sync::Arc<pty::PtyMaster>, crate::fs::FileSystemError> {
        pty::open_master(123, 456)
    }
    pub(crate) fn slave(
        index: u32,
    ) -> Result<alloc::sync::Arc<pty::PtySlave>, crate::fs::FileSystemError> {
        pty::open_slave(index)
    }
    pub(crate) fn boot() {
        static BOOT: std::sync::Once = std::sync::Once::new();
        BOOT.call_once(|| pty::init().unwrap());
    }
}
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn boot() -> std::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap();
    namespace::boot();
    tty::boot();
    guard
}
#[test]
fn actual_fput_hangs_up_and_reuses_index_while_backend_observers_still_exist() {
    let _serial = boot();
    let master = tty::master().unwrap();
    let index = master.index();
    master.set_locked(false);
    let slave = tty::slave(index).unwrap();
    let master_observer = master.clone();
    let slave_observer = slave.clone();
    let generation = slave.readiness_generation();
    let hangups = tty::HANGUPS.load(core::sync::atomic::Ordering::Relaxed);
    master.close();
    assert!(slave.master_hung_up());
    assert!(slave.readiness_generation() > generation);
    assert!(slave.write(b"closed").is_err());
    assert!(!tty::pty::slave_exists(index));
    assert!(tty::slave(index).is_err());
    assert_eq!(
        tty::HANGUPS.load(core::sync::atomic::Ordering::Relaxed),
        hangups + 1
    );
    let competitor = tty::master().unwrap();
    assert_ne!(
        competitor.index(),
        index,
        "live slave admission retains old index"
    );
    slave.close();
    let replacement = tty::master().unwrap();
    assert_eq!(replacement.index(), index);
    replacement.set_locked(false);
    let replacement_slave = tty::slave(index).unwrap();
    master.close();
    slave.close();
    drop(master);
    drop(slave);
    drop(master_observer);
    drop(slave_observer);
    assert!(!replacement_slave.master_hung_up());
    assert_eq!(
        tty::HANGUPS.load(core::sync::atomic::Ordering::Relaxed),
        hangups + 1
    );
    replacement_slave.close();
    replacement.close();
    competitor.close();
}
#[test]
fn last_actual_slave_close_publishes_hup_and_a_new_open_clears_it() {
    let _serial = boot();
    let master = tty::master().unwrap();
    master.set_locked(false);
    let first = tty::slave(master.index()).unwrap();
    let second = tty::slave(master.index()).unwrap();
    let observer = first.clone();
    first.close();
    assert!(!master.peer_hung_up());
    second.close();
    assert!(master.peer_hung_up());
    assert!(master.write(b"orphan").is_err());
    let replacement = tty::slave(master.index()).unwrap();
    drop(observer);
    drop(first);
    drop(second);
    assert!(!master.peer_hung_up());
    assert_eq!(master.write(b"hello\n").unwrap(), 6);
    replacement.close();
    master.close();
}
#[test]
fn allocation_failure_never_publishes_a_partial_pair_or_slave_admission() {
    let _serial = boot();
    let master = tty::master().unwrap();
    master.set_locked(false);
    let before = tty::pty::slave_indices().unwrap();
    allocation_failure::fail_next();
    assert!(matches!(
        tty::slave(master.index()),
        Err(fs::FileSystemError::OutOfMemory)
    ));
    assert!(allocation_failure::was_consumed());
    assert!(master.peer_hung_up());
    let mut failures = 0;
    for stage in 0..32 {
        allocation_failure::fail_after(stage);
        let result = tty::master();
        let consumed = allocation_failure::was_consumed();
        match result {
            Err(fs::FileSystemError::OutOfMemory) => {
                assert!(consumed);
                failures += 1;
            }
            Ok(pair) => {
                assert!(!consumed);
                pair.close();
                break;
            }
            Err(other) => panic!("unexpected preparation error: {other:?}"),
        }
        assert_eq!(tty::pty::slave_indices().unwrap(), before);
    }
    assert!(
        failures >= 8,
        "exercise endpoint, terminal, pair and index preparation"
    );
    assert_eq!(tty::pty::slave_indices().unwrap(), before);
    assert_eq!(tty::pty::slave_owner(master.index()), Some((123, 456)));
    master.close();
}
