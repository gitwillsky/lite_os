//! Actual production filters, notification transactions and TaskEvent; explicit host fd/MM/scheduler seams.
#![feature(allocator_ext)]
#![allow(dead_code, unused_imports)]
extern crate alloc;
#[allow(dead_code, unused_imports)]
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[allow(dead_code, unused_imports)]
#[path = "../src/namespace_fixture.rs"]
mod namespace;

use alloc::sync::Arc;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Condvar, Mutex},
};
use syscall_abi::{errno, seccomp::*};
macro_rules! info { ($($args:tt)*) => { let _ = format_args!($($args)*); }; }
#[path = "../src/seccomp_environment.rs"]
mod seccomp_environment;
use seccomp_environment::{file, fs, id, ipc};
#[path = "../src/interruptible_event_target.rs"]
mod scheduler_target;
#[path = "../src/sync.rs"]
mod sync;
mod arch {
    pub(crate) mod user {
        pub(crate) const AUDIT_ARCH: u32 = 0xc000_00b7;
    }
}
#[path = "seccomp/notifications.rs"]
mod model;
use fs::device::{DeviceError, DeviceFile, IoctlCall, UserFault, UserMemory};
use model::seccomp::{SeccompDecision, SeccompError, ThreadSecurityOwner, ValidatedFilter};

#[path = "seccomp/notification_copyout.rs"]
mod notification_copyout;

struct HostFiles {
    entries: BTreeMap<usize, Arc<file::OpenFileDescription>>,
    installed: Vec<(usize, usize, bool)>,
    fail: bool,
}
struct InstallHandoff {
    entered: AtomicBool,
    release: (Mutex<bool>, Condvar),
}
impl InstallHandoff {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: false.into(),
            release: (Mutex::new(false), Condvar::new()),
        })
    }
    fn wait_entered(&self) {
        // Bound rendezvous, never sleep an arbitrary amount to infer a production transaction state.
        let mut released = self.release.0.lock().unwrap();
        while !self.entered.load(Ordering::Acquire) {
            released = self.release.1.wait(released).unwrap();
        }
    }
    fn enter(&self) {
        let mut released = self.release.0.lock().unwrap();
        self.entered.store(true, Ordering::Release);
        self.release.1.notify_all();
        while !*released {
            released = self.release.1.wait(released).unwrap();
        }
    }
    fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
}
pub(crate) struct TaskControlBlock {
    pid: namespace::PidRegistration,
    security: ThreadSecurityOwner,
    files: Mutex<HostFiles>,
    target: Arc<scheduler_target::Target>,
    handoff: Option<Arc<InstallHandoff>>,
}
impl TaskControlBlock {
    fn new(tid: usize, handoff: Option<Arc<InstallHandoff>>) -> Arc<Self> {
        let owner = namespace::boot();
        let view = namespace::initial_pid().child(owner, 0).unwrap();
        Self::in_namespace(tid, view, handoff)
    }
    fn in_namespace(
        key: usize,
        view: Arc<namespace::PidNamespace>,
        handoff: Option<Arc<InstallHandoff>>,
    ) -> Arc<Self> {
        let mut pid =
            namespace::PreparedPid::try_new(key, view, &[], &crate::namespace::root_authority())
                .unwrap();
        pid.publish().unwrap();
        Arc::new(Self {
            pid: pid.take_registration(),
            security: ThreadSecurityOwner::default(),
            target: scheduler_target::Target::new(),
            handoff,
            files: Mutex::new(HostFiles {
                entries: BTreeMap::new(),
                installed: Vec::new(),
                fail: false,
            }),
        })
    }
    fn tid(&self) -> usize {
        self.pid.identity().key()
    }
    fn tid_identity(&self) -> Arc<namespace::PidIdentity> {
        self.pid.identity().clone()
    }
    fn thread(&self) -> &Self {
        self
    }
    fn user_program_counter(&self) -> usize {
        0x1234_5678_abcd_0004
    }
    fn fd_get(&self, fd: usize) -> Option<Arc<file::OpenFileDescription>> {
        self.files.lock().unwrap().entries.get(&fd).cloned()
    }
    fn fd_receive(
        &self,
        file: Arc<file::OpenFileDescription>,
        destination: Option<usize>,
        cloexec: bool,
    ) -> Result<usize, file::FileDescriptorError> {
        assert!(
            task::current_task().is_some_and(|current| core::ptr::eq(current.as_ref(), self)),
            "observer must not inject by operating another task's table"
        );
        if let Some(handoff) = &self.handoff {
            handoff.enter();
        }
        let mut files = self.files.lock().unwrap();
        if files.fail {
            return Err(file::FileDescriptorError::Limit);
        }
        let fd = destination.unwrap_or(7);
        files.installed.push((fd, file.identity, cloexec));
        files.entries.insert(fd, file);
        Ok(fd)
    }
}
thread_local! { static CURRENT: RefCell<Option<Arc<TaskControlBlock>>> = const { RefCell::new(None) }; }
mod task {
    pub(crate) fn current_task() -> Option<alloc::sync::Arc<super::TaskControlBlock>> {
        super::CURRENT.with_borrow(Clone::clone)
    }
}
struct Binding {
    _event: scheduler_target::Binding,
}
impl Drop for Binding {
    fn drop(&mut self) {
        CURRENT.with_borrow_mut(Option::take);
    }
}
fn bind(task: &Arc<TaskControlBlock>) -> Binding {
    CURRENT.with_borrow_mut(|slot| assert!(slot.replace(task.clone()).is_none()));
    Binding {
        _event: task.target.bind(),
    }
}
struct PipeScheduler {
    epoch: Mutex<u64>,
    changed: Condvar,
}
static PIPES: PipeScheduler = PipeScheduler {
    epoch: Mutex::new(0),
    changed: Condvar::new(),
};
impl ipc::PipeScheduler for PipeScheduler {
    fn notify(&self, _: &Arc<ipc::Pipe>) {
        *self.epoch.lock().unwrap() += 1;
        self.changed.notify_all();
    }
    fn wait(
        &self,
        pipe: &Arc<ipc::Pipe>,
        condition: ipc::PipeWaitCondition,
        _: Option<u64>,
    ) -> sync::WaitResult {
        let mut epoch = self.epoch.lock().unwrap();
        while !pipe.poll_state(condition.direction()).satisfies(condition) {
            epoch = self.changed.wait(epoch).unwrap();
        }
        sync::WaitResult::Woken
    }
}
fn initialize_pipe_scheduler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| ipc::install_pipe_scheduler(&PIPES));
}
struct Memory {
    pid_namespace: Arc<namespace::PidNamespace>,
    bytes: Mutex<Vec<u8>>,
    fault: AtomicBool,
}
impl Memory {
    fn new(bytes: Vec<u8>) -> Self {
        namespace::boot();
        Self::in_namespace(bytes, namespace::initial_pid())
    }
    fn in_namespace(bytes: Vec<u8>, pid_namespace: Arc<namespace::PidNamespace>) -> Self {
        Self {
            pid_namespace,
            bytes: Mutex::new(bytes),
            fault: false.into(),
        }
    }
    fn id(&self) -> u64 {
        u64::from_ne_bytes(self.bytes.lock().unwrap()[..8].try_into().unwrap())
    }
}
impl UserMemory for Memory {
    fn pid_namespace(&self) -> Arc<namespace::PidNamespace> {
        self.pid_namespace.clone()
    }
    fn read(&self, address: usize, bytes: &mut [u8]) -> Result<(), UserFault> {
        let offset = address.checked_sub(1).ok_or(UserFault)?;
        let contents = self.bytes.lock().unwrap();
        let source = contents
            .get(offset..offset.checked_add(bytes.len()).ok_or(UserFault)?)
            .ok_or(UserFault)?;
        bytes.copy_from_slice(source);
        Ok(())
    }
    fn write(&self, address: usize, bytes: &[u8]) -> Result<(), UserFault> {
        if self.fault.load(Ordering::Acquire) {
            return Err(UserFault);
        }
        let offset = address.checked_sub(1).ok_or(UserFault)?;
        let mut contents = self.bytes.lock().unwrap();
        let target = contents
            .get_mut(offset..offset.checked_add(bytes.len()).ok_or(UserFault)?)
            .ok_or(UserFault)?;
        target.copy_from_slice(bytes);
        Ok(())
    }
}
fn ioctl(
    file: &Arc<dyn DeviceFile>,
    request: usize,
    memory: &Memory,
) -> Result<isize, DeviceError> {
    file.ioctl(&IoctlCall {
        fd_readable: true,
        request,
        argument: 1,
        user: memory,
        nonblocking: false,
    })
}
fn action(value: u32) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[0] = 6;
    bytes[4..].copy_from_slice(&value.to_ne_bytes());
    bytes
}
fn install(task: &TaskControlBlock, value: u32, flags: u32) -> Option<Arc<dyn DeviceFile>> {
    let validated = ValidatedFilter::prepare(&action(value), flags).unwrap();
    let listener = validated.listener().unwrap();
    let prepared = task.security.prepare_filter(&validated).unwrap();
    assert!(prepared.matches(&task.security));
    drop(prepared.publish(&task.security, false));
    listener
}
fn spawn_request(
    task: &Arc<TaskControlBlock>,
) -> std::thread::JoinHandle<Result<Option<isize>, SeccompError>> {
    let task = task.clone();
    std::thread::spawn(move || {
        let _binding = bind(&task);
        let SeccompDecision::Notify {
            queue,
            data,
            killable_after_receive,
        } = task.evaluate_seccomp(172, [usize::MAX, 2, 3, 4, 5, 6])
        else {
            panic!("expected actual notification");
        };
        task.wait_seccomp_notification(&queue, data, killable_after_receive)
    })
}
fn receive(listener: &Arc<dyn DeviceFile>, task: &TaskControlBlock) -> Memory {
    task.target.wait_until_sleeping();
    let received = Memory::new(vec![0; 80]);
    assert_eq!(ioctl(listener, IOCTL_NOTIF_RECV, &received), Ok(0));
    received
}
fn response(id: u64, value: i64, error: i32, flags: u32) -> Memory {
    let mut bytes = vec![0; 24];
    bytes[..8].copy_from_slice(&id.to_ne_bytes());
    bytes[8..16].copy_from_slice(&value.to_ne_bytes());
    bytes[16..20].copy_from_slice(&error.to_ne_bytes());
    bytes[20..].copy_from_slice(&flags.to_ne_bytes());
    Memory::new(bytes)
}
fn addfd(id: u64, flags: u32, source: u32, destination: u32, cloexec: bool) -> Memory {
    let mut bytes = vec![0; 24];
    bytes[..8].copy_from_slice(&id.to_ne_bytes());
    bytes[8..12].copy_from_slice(&flags.to_ne_bytes());
    bytes[12..16].copy_from_slice(&source.to_ne_bytes());
    bytes[16..20].copy_from_slice(&destination.to_ne_bytes());
    bytes[20..].copy_from_slice(&(if cloexec { fs::O_CLOEXEC } else { 0 }).to_ne_bytes());
    Memory::new(bytes)
}
#[test]
fn receive_data_error_reply_continue_and_stale_id() {
    initialize_pipe_scheduler();
    let task = TaskControlBlock::new(41, None);
    let listener = install(&task, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    assert_eq!(listener.anonymous_name(), b"anon_inode:seccomp notify");
    for (value, error, flags, expected) in [
        (81, 0, 0, Some(81)),
        (93, -13, 0, Some(-13)),
        (0, 0, USER_NOTIF_FLAG_CONTINUE, None),
    ] {
        let worker = spawn_request(&task);
        let received = receive(&listener, &task);
        let bytes = received.bytes.lock().unwrap();
        assert_eq!(
            u32::from_ne_bytes(bytes[8..12].try_into().unwrap()),
            task.tid_identity().number_in(&namespace::initial_pid())
        );
        assert_eq!(u32::from_ne_bytes(bytes[16..20].try_into().unwrap()), 172);
        assert_eq!(
            u64::from_ne_bytes(bytes[32..40].try_into().unwrap()),
            u64::MAX
        );
        drop(bytes);
        let id = received.id();
        let valid = Memory::new(id.to_ne_bytes().to_vec());
        assert_eq!(ioctl(&listener, IOCTL_NOTIF_ID_VALID, &valid), Ok(0));
        assert_eq!(
            ioctl(
                &listener,
                IOCTL_NOTIF_SEND,
                &response(id, value, error, flags)
            ),
            Ok(0)
        );
        assert_eq!(worker.join().unwrap(), Ok(expected));
        assert_eq!(
            ioctl(&listener, IOCTL_NOTIF_ID_VALID, &valid),
            Err(DeviceError::Errno(errno::ENOENT))
        );
    }
}
#[test]
fn receive_copy_fault_restores_init_and_closed_listener_completes_enosys() {
    initialize_pipe_scheduler();
    let task = TaskControlBlock::new(42, None);
    let listener = install(&task, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    let worker = spawn_request(&task);
    task.target.wait_until_sleeping();
    let memory = Memory::new(vec![0; 80]);
    memory.fault.store(true, Ordering::Release);
    assert_eq!(
        ioctl(&listener, IOCTL_NOTIF_RECV, &memory),
        Err(DeviceError::Errno(errno::EFAULT))
    );
    assert_ne!(listener.poll(1) & 1, 0);
    memory.fault.store(false, Ordering::Release);
    assert_eq!(ioctl(&listener, IOCTL_NOTIF_RECV, &memory), Ok(0));
    drop(listener);
    assert_eq!(worker.join().unwrap(), Ok(Some(-errno::ENOSYS)));
}
#[test]
fn actual_listener_fput_completes_request_despite_live_backend_observer() {
    initialize_pipe_scheduler();
    let task = TaskControlBlock::new(424, None);
    let listener = install(&task, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    let worker = spawn_request(&task);
    task.target.wait_until_sleeping();
    let observer = listener.clone();
    listener.release();
    assert_eq!(worker.join().unwrap(), Ok(Some(-errno::ENOSYS)));
    listener.release();
    drop(listener);
    let next = spawn_request(&task);
    assert_eq!(next.join().unwrap(), Ok(Some(-errno::ENOSYS)));
    drop(observer);
}
#[test]
fn cancelled_notification_cannot_be_replied_or_validated() {
    initialize_pipe_scheduler();
    let task = TaskControlBlock::new(43, None);
    let listener = install(&task, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    let worker = spawn_request(&task);
    let received = receive(&listener, &task);
    task.target.signal();
    assert_eq!(worker.join().unwrap(), Err(SeccompError::Interrupted));
    assert_eq!(
        ioctl(
            &listener,
            IOCTL_NOTIF_SEND,
            &response(received.id(), 1, 0, 0)
        ),
        Err(DeviceError::Errno(errno::ENOENT))
    );
    task.target.clear_signal();
}
#[test]
fn task_membership_hup_ignores_retained_tcb_policy_and_evaluation_arcs() {
    initialize_pipe_scheduler();
    let task = TaskControlBlock::new(44, None);
    let listener = install(&task, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    let held = task.evaluate_seccomp(172, [0; 6]);
    let next = ValidatedFilter::prepare(&action(RET_ALLOW), 0).unwrap();
    let prepared = task.security.prepare_filter(&next).unwrap();
    assert_eq!(listener.poll(16), 0);
    task.security.retire();
    assert_eq!(listener.poll(16), 16);
    assert_eq!(
        ioctl(&listener, IOCTL_NOTIF_RECV, &Memory::new(vec![0; 80])),
        Err(DeviceError::Errno(errno::ENOENT))
    );
    drop((prepared, held));
    assert_eq!(listener.poll(16), 16);
}
#[test]
fn addfd_send_installs_in_actual_target_and_replies_with_the_new_fd() {
    initialize_pipe_scheduler();
    let target = TaskControlBlock::new(45, None);
    let observer = TaskControlBlock::new(90, None);
    let listener = install(&target, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    let worker = spawn_request(&target);
    let received = receive(&listener, &target);
    observer.files.lock().unwrap().entries.insert(
        3,
        Arc::new(file::OpenFileDescription {
            identity: 717,
            device: None,
        }),
    );
    let _binding = bind(&observer);
    assert_eq!(
        ioctl(
            &listener,
            IOCTL_NOTIF_ADDFD,
            &addfd(
                received.id(),
                ADDFD_FLAG_SETFD | ADDFD_FLAG_SEND,
                3,
                12,
                true
            )
        ),
        Ok(12)
    );
    assert_eq!(worker.join().unwrap(), Ok(Some(12)));
    assert_eq!(
        target.files.lock().unwrap().installed,
        vec![(12, 717, true)]
    );
    assert!(observer.files.lock().unwrap().installed.is_empty());
}
#[test]
fn processing_interruption_waits_for_real_failure_then_allows_another_response() {
    initialize_pipe_scheduler();
    let handoff = InstallHandoff::new();
    let target = TaskControlBlock::new(46, Some(handoff.clone()));
    let observer = TaskControlBlock::new(91, None);
    target.files.lock().unwrap().fail = true;
    let listener = install(&target, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    let worker = spawn_request(&target);
    let received = receive(&listener, &target);
    let id = received.id();
    observer.files.lock().unwrap().entries.insert(
        3,
        Arc::new(file::OpenFileDescription {
            identity: 718,
            device: None,
        }),
    );
    let injected_listener = listener.clone();
    let injected_observer = observer.clone();
    let injector = std::thread::spawn(move || {
        let _binding = bind(&injected_observer);
        ioctl(
            &injected_listener,
            IOCTL_NOTIF_ADDFD,
            &addfd(id, ADDFD_FLAG_SEND, 3, 0, false),
        )
    });
    handoff.wait_entered();
    observer.target.wait_until_sleeping();
    observer.target.signal();
    handoff.release();
    assert_eq!(injector.join().unwrap(), Ok(-errno::EMFILE));
    assert_eq!(
        ioctl(&listener, IOCTL_NOTIF_SEND, &response(id, 33, 0, 0)),
        Ok(0)
    );
    assert_eq!(worker.join().unwrap(), Ok(Some(33)));
}

#[test]
fn killable_after_receive_preserves_caught_signals_and_terminates_on_fatal_signal() {
    initialize_pipe_scheduler();
    let task = TaskControlBlock::new(47, None);
    let listener = install(
        &task,
        RET_USER_NOTIF,
        FILTER_FLAG_NEW_LISTENER | FILTER_FLAG_WAIT_KILLABLE_RECV,
    )
    .unwrap();
    let worker = spawn_request(&task);
    let received = receive(&listener, &task);
    task.target.signal();
    task.target.wait_until_sleeping();
    assert_eq!(
        ioctl(
            &listener,
            IOCTL_NOTIF_ID_VALID,
            &Memory::new(received.id().to_ne_bytes().to_vec())
        ),
        Ok(0)
    );
    assert_eq!(
        ioctl(
            &listener,
            IOCTL_NOTIF_SEND,
            &response(received.id(), 66, 0, 0)
        ),
        Ok(0)
    );
    assert_eq!(worker.join().unwrap(), Ok(Some(66)));
    task.target.clear_signal();
    let worker = spawn_request(&task);
    let received = receive(&listener, &task);
    task.target.fatal_signal();
    assert_eq!(worker.join().unwrap(), Err(SeccompError::Interrupted));
    assert_eq!(
        ioctl(
            &listener,
            IOCTL_NOTIF_ID_VALID,
            &Memory::new(received.id().to_ne_bytes().to_vec())
        ),
        Err(DeviceError::Errno(errno::ENOENT))
    );
}
#[test]
fn duplicate_listener_checks_actual_lifetime_and_newest_equal_action_selects_its_filter() {
    initialize_pipe_scheduler();
    let task = TaskControlBlock::new(48, None);
    let listener = install(&task, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    let duplicate = ValidatedFilter::prepare(&action(RET_ALLOW), FILTER_FLAG_NEW_LISTENER).unwrap();
    assert!(matches!(
        task.security.prepare_filter(&duplicate),
        Err(SeccompError::Busy)
    ));
    // A newer equal USER_NOTIF filter without a listener wins, even though an older listener exists.
    install(&task, RET_USER_NOTIF, 0);
    assert_eq!(
        task.evaluate_seccomp(172, [0; 6]),
        SeccompDecision::NoObserver
    );
    drop(listener);
    let new_listener = duplicate.listener().unwrap().unwrap();
    let prepared = task.security.prepare_filter(&duplicate).unwrap();
    drop(prepared.publish(&task.security, false));
    assert!(matches!(
        task.evaluate_seccomp(172, [0; 6]),
        SeccompDecision::NoObserver
    )); // newest ALLOW cannot relax older USER_NOTIF
    drop(new_listener);
}
#[test]
fn listener_flags_are_scalar_and_log_uses_the_selected_action() {
    initialize_pipe_scheduler();
    let task = TaskControlBlock::new(49, None);
    let listener = install(&task, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    let unused = Memory::new(Vec::new());
    assert_eq!(
        listener.ioctl(&IoctlCall {
            fd_readable: true,
            request: IOCTL_NOTIF_SET_FLAGS,
            argument: USER_NOTIF_FD_SYNC_WAKE_UP,
            user: &unused,
            nonblocking: false
        }),
        Ok(0)
    );
    assert_eq!(
        listener.ioctl(&IoctlCall {
            fd_readable: true,
            request: IOCTL_NOTIF_SET_FLAGS,
            argument: 2,
            user: &unused,
            nonblocking: false
        }),
        Err(DeviceError::Errno(errno::EINVAL))
    );
    task.security.retire();
    let logged = TaskControlBlock::new(50, None);
    install(
        &logged,
        RET_ERRNO | 13,
        FILTER_FLAG_LOG | FILTER_FLAG_SPEC_ALLOW,
    );
    let mut observed = None;
    assert_eq!(
        logged.security.evaluate(
            172,
            [0; 6],
            arch::user::AUDIT_ARCH,
            || 1,
            |action| observed = Some(action)
        ),
        SeccompDecision::Errno(13)
    );
    assert_eq!(observed, Some(RET_ERRNO));
    install(&logged, RET_ALLOW, FILTER_FLAG_LOG);
    let mut observed = None;
    assert_eq!(
        logged.security.evaluate(
            172,
            [0; 6],
            arch::user::AUDIT_ARCH,
            || 1,
            |action| observed = Some(action)
        ),
        SeccompDecision::Errno(13)
    );
    assert_eq!(observed, Some(RET_ERRNO));
}

#[test]
fn reply_during_one_install_cancels_other_queued_addfd_instead_of_publishing_it() {
    initialize_pipe_scheduler();
    let handoff = InstallHandoff::new();
    let target = TaskControlBlock::new(51, Some(handoff.clone()));
    let first = TaskControlBlock::new(92, None);
    let second = TaskControlBlock::new(93, None);
    let listener = install(&target, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    let worker = spawn_request(&target);
    let received = receive(&listener, &target);
    let id = received.id();
    first.files.lock().unwrap().entries.insert(
        3,
        Arc::new(file::OpenFileDescription {
            identity: 1001,
            device: None,
        }),
    );
    second.files.lock().unwrap().entries.insert(
        4,
        Arc::new(file::OpenFileDescription {
            identity: 1002,
            device: None,
        }),
    );
    let first_listener = listener.clone();
    let first_caller = first.clone();
    let first_injection = std::thread::spawn(move || {
        let _binding = bind(&first_caller);
        ioctl(
            &first_listener,
            IOCTL_NOTIF_ADDFD,
            &addfd(id, 0, 3, 0, false),
        )
    });
    handoff.wait_entered();
    let second_listener = listener.clone();
    let second_caller = second.clone();
    let second_injection = std::thread::spawn(move || {
        let _binding = bind(&second_caller);
        ioctl(
            &second_listener,
            IOCTL_NOTIF_ADDFD,
            &addfd(id, 0, 4, 0, false),
        )
    });
    second.target.wait_until_sleeping(); // The second actual command is queued, not merely prepared.
    assert_eq!(
        ioctl(&listener, IOCTL_NOTIF_SEND, &response(id, 88, 0, 0)),
        Ok(0)
    );
    handoff.release();
    assert_eq!(first_injection.join().unwrap(), Ok(7));
    assert_eq!(second_injection.join().unwrap(), Ok(-errno::ESRCH));
    assert_eq!(worker.join().unwrap(), Ok(Some(88)));
    assert_eq!(
        target.files.lock().unwrap().installed,
        vec![(7, 1001, false)]
    );
}

#[test]
fn send_reservation_queued_during_ordinary_install_is_not_a_completed_response() {
    initialize_pipe_scheduler();
    let handoff = InstallHandoff::new();
    let target = TaskControlBlock::new(52, Some(handoff.clone()));
    let first = TaskControlBlock::new(94, None);
    let second = TaskControlBlock::new(95, None);
    let listener = install(&target, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    let worker = spawn_request(&target);
    let received = receive(&listener, &target);
    let id = received.id();
    first.files.lock().unwrap().entries.insert(
        3,
        Arc::new(file::OpenFileDescription {
            identity: 2001,
            device: None,
        }),
    );
    second.files.lock().unwrap().entries.insert(
        4,
        Arc::new(file::OpenFileDescription {
            identity: 2002,
            device: None,
        }),
    );
    let first_listener = listener.clone();
    let first_caller = first.clone();
    let first_injection = std::thread::spawn(move || {
        let _binding = bind(&first_caller);
        ioctl(
            &first_listener,
            IOCTL_NOTIF_ADDFD,
            &addfd(id, 0, 3, 0, false),
        )
    });
    handoff.wait_entered();
    let second_listener = listener.clone();
    let second_caller = second.clone();
    let second_injection = std::thread::spawn(move || {
        let _binding = bind(&second_caller);
        ioctl(
            &second_listener,
            IOCTL_NOTIF_ADDFD,
            &addfd(id, ADDFD_FLAG_SETFD | ADDFD_FLAG_SEND, 4, 13, false),
        )
    });
    second.target.wait_until_sleeping();
    assert_eq!(
        ioctl(&listener, IOCTL_NOTIF_SEND, &response(id, 88, 0, 0)),
        Err(DeviceError::Errno(errno::EINPROGRESS))
    );
    handoff.release();
    assert_eq!(first_injection.join().unwrap(), Ok(7));
    assert_eq!(second_injection.join().unwrap(), Ok(13));
    assert_eq!(worker.join().unwrap(), Ok(Some(13)));
    assert_eq!(
        target.files.lock().unwrap().installed,
        vec![(7, 2001, false), (13, 2002, false)]
    );
}

#[test]
fn receive_projects_actual_tid_in_each_receivers_namespace_after_listener_transfer() {
    initialize_pipe_scheduler();
    let owner = namespace::boot();
    let view = namespace::initial_pid().child(owner.clone(), 0).unwrap();
    let target = TaskControlBlock::in_namespace(2001, view.clone(), None);
    let sibling = namespace::initial_pid().child(owner.clone(), 0).unwrap();
    let descendant = view.child(owner, 0).unwrap();
    let listener = install(&target, RET_USER_NOTIF, FILTER_FLAG_NEW_LISTENER).unwrap();
    for receiver_view in [namespace::initial_pid(), view, sibling, descendant] {
        let worker = spawn_request(&target);
        target.target.wait_until_sleeping();
        let memory = Memory::in_namespace(vec![0; 80], receiver_view.clone());
        assert_eq!(ioctl(&listener, IOCTL_NOTIF_RECV, &memory), Ok(0));
        let number = u32::from_ne_bytes(memory.bytes.lock().unwrap()[8..12].try_into().unwrap());
        assert_eq!(number, target.tid_identity().number_in(&receiver_view));
        assert_eq!(
            ioctl(
                &listener,
                IOCTL_NOTIF_SEND,
                &response(memory.id(), 17, 0, 0)
            ),
            Ok(0)
        );
        assert_eq!(worker.join().unwrap(), Ok(Some(17)));
    }
}
