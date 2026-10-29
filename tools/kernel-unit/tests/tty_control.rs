#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际 Terminal control/foreground/retirement owner；物理 Console 是显式无输入 adapter seam。
extern crate alloc;
use alloc::sync::Arc;
#[path = "../../../kernel/src/fs/devnum.rs"]
mod device_number;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;

mod fs {
    #[derive(Debug)]
    pub(crate) enum FileSystemError {
        IoError,
    }
    pub(crate) use crate::device_number::DeviceNumber;
}

use crate::devices::tty::terminal;
struct Console;
impl terminal::Console for Console {
    fn read(&self, _: &mut [u8]) -> Result<usize, fs::FileSystemError> {
        Ok(0)
    }
    fn input_ready(&self) -> bool {
        false
    }
    fn discard_input(&self) -> usize {
        0
    }
    fn discard_output(&self) -> usize {
        0
    }
    fn write(&self, bytes: &[u8]) -> Result<usize, fs::FileSystemError> {
        Ok(bytes.len())
    }
}
fn identities() -> (
    Vec<Arc<namespace::PidIdentity>>,
    Vec<namespace::PidRegistration>,
) {
    namespace::boot();
    let view = namespace::initial_pid()
        .child(namespace::initial_user(), 0)
        .unwrap();
    let mut identities = Vec::new();
    let mut registrations = Vec::new();
    for key in 100..103 {
        let mut prepared =
            namespace::PreparedPid::try_new(key, view.clone(), &[], &namespace::root_authority())
                .unwrap();
        prepared.publish().unwrap();
        identities.push(prepared.identity());
        registrations.push(prepared.take_registration());
    }
    (identities, registrations)
}
#[test]
fn repeat_claim_preserves_foreground_and_failed_claim_leaves_control_unchanged() {
    let (pids, _registrations) = identities();
    let tty = terminal::Terminal::new(Arc::new(Console), fs::DeviceNumber::new(136, 3)).unwrap();
    let retired = tty
        .claim_session(pids[0].clone(), pids[0].clone(), false)
        .unwrap();
    assert!(retired.previous_session().is_none());
    tty.set_foreground_pgid(pids[0].key(), pids[1].clone())
        .unwrap();
    drop(
        tty.claim_session(pids[0].clone(), pids[0].clone(), false)
            .unwrap(),
    );
    assert_eq!(
        tty.control_ids(&pids[0].namespace()).1,
        pids[1].number_in(&pids[0].namespace()) as i32
    );
    assert!(
        tty.claim_session(pids[2].clone(), pids[2].clone(), false)
            .is_err()
    );
    assert_eq!(tty.controlling_session(), Some(pids[0].key()));
    assert!(tty.release_session(pids[2].key()).is_none());
    let retired = tty
        .claim_session(pids[2].clone(), pids[2].clone(), true)
        .unwrap();
    assert_eq!(retired.previous_session(), Some(pids[0].key()));
    assert_eq!(retired.foreground_group(), Some(pids[1].key()));
    assert_eq!(tty.controlling_session(), Some(pids[2].key()));
    let released = tty.release_session(pids[2].key()).unwrap();
    assert_eq!(released.previous_session(), Some(pids[2].key()));
    assert!(tty.controlling_session().is_none());
    assert_eq!(
        tty.proc_identity(&pids[0].namespace()),
        (fs::DeviceNumber::new(136, 3).encode() as u32, 0)
    );
}
#[test]
fn retired_control_keeps_historical_pid_alive_only_until_caller_retires_it() {
    let (mut pids, mut registrations) = identities();
    let tty = terminal::Terminal::new(Arc::new(Console), fs::DeviceNumber::new(5, 1)).unwrap();
    let observer = Arc::downgrade(&pids[0]);
    drop(
        tty.claim_session(pids[0].clone(), pids[0].clone(), false)
            .unwrap(),
    );
    let control = tty.hangup();
    drop(pids.remove(0));
    drop(registrations.remove(0));
    assert!(observer.upgrade().is_some());
    assert!(tty.controlling_session().is_none());
    drop(control);
    assert!(observer.upgrade().is_none());
}

#[path = "."]
pub(crate) mod execution {
    #[path = "../src/sync.rs"]
    pub(crate) mod sync;
}

#[path = "."]
pub(crate) mod devices {
    #[path = "../../../kernel/src/devices/tty"]
    pub(crate) mod tty {
        #[allow(
            unused_imports,
            reason = "focused TTY fixture consumes a subset of the terminal facade"
        )]
        pub(crate) mod terminal;
    }
}
