use core::{
    hint::spin_loop,
    sync::atomic::{AtomicUsize, Ordering},
};

use spin::{Mutex, MutexGuard, Once};

use super::NetworkStack;

// OWNER: the IPv4 module uniquely owns interface configuration, routes, ARP cache and SocketSet.
// The Option is an exclusive poll loan, not a second state: only a protocol writer may take it and
// must restore the same value before releasing membership. Without the loan, device callbacks
// would execute under this mutex and serialize endpoint metadata for the whole MMIO duration.
pub(super) struct NetworkStackOwner {
    state: Mutex<Option<NetworkStack>>,
}

pub(super) struct NetworkStackGuard<'a>(MutexGuard<'a, Option<NetworkStack>>);

/// 唯一 NetworkStack 的 exclusive poll loan；Drop 固定先归还 stack，再释放 writer membership。
pub(super) struct NetworkPollLoan<'a> {
    owner: &'a NetworkStackOwner,
    stack: Option<NetworkStack>,
    _protocol: ProtocolWriteGuard,
}

impl core::ops::Deref for NetworkStackGuard<'_> {
    type Target = NetworkStack;

    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .expect("protocol stack accessed during exclusive poll loan")
    }
}

impl core::ops::DerefMut for NetworkStackGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
            .as_mut()
            .expect("protocol stack accessed during exclusive poll loan")
    }
}

impl core::ops::Deref for NetworkPollLoan<'_> {
    type Target = NetworkStack;

    fn deref(&self) -> &Self::Target {
        self.stack
            .as_ref()
            .expect("poll loan lost its protocol stack")
    }
}

impl core::ops::DerefMut for NetworkPollLoan<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.stack
            .as_mut()
            .expect("poll loan lost its protocol stack")
    }
}

impl Drop for NetworkPollLoan<'_> {
    fn drop(&mut self) {
        let stack = self.stack.take().expect("poll loan restored twice");
        let mut state = self.owner.state.lock();
        assert!(state.is_none(), "protocol stack restored over live owner");
        *state = Some(stack);
        // `_protocol` 在本 Drop 返回后释放；反序会让另一 writer 观察到空 owner slot。
    }
}

impl NetworkStackOwner {
    pub(super) fn new(stack: NetworkStack) -> Self {
        Self {
            state: Mutex::new(Some(stack)),
        }
    }

    pub(super) fn lock(&self) -> NetworkStackGuard<'_> {
        NetworkStackGuard(self.state.lock())
    }

    pub(super) fn poll_loan(&self) -> NetworkPollLoan<'_> {
        let protocol = protocol_write();
        let stack = self
            .state
            .lock()
            .take()
            .expect("protocol stack loaned twice");
        NetworkPollLoan {
            owner: self,
            stack: Some(stack),
            _protocol: protocol,
        }
    }
}

// OWNER: protocol_owner uniquely owns the one IPv4 stack storage slot.
pub(super) static NETWORK_STACK: Once<NetworkStackOwner> = Once::new();

// OWNER: one atomic word owns shared endpoint memberships and the exclusive poll bit. Readers copy
// only endpoint-local payload; writer publication blocks new readers then waits for existing loans.
// Without the writer bit, Interface could process a closed placeholder and lose ingress. A leaked
// guard would stop network progress, so both guard types release their exact membership in Drop.
const PROTOCOL_WRITER: usize = 1usize << (usize::BITS - 1);
// OWNER: protocol_owner uniquely owns all endpoint-reader and poll-writer memberships.
static PROTOCOL_GATE: AtomicUsize = AtomicUsize::new(0);

pub(super) struct ProtocolReadGuard;

impl Drop for ProtocolReadGuard {
    fn drop(&mut self) {
        let previous = PROTOCOL_GATE.fetch_sub(1, Ordering::Release);
        debug_assert!(previous & !PROTOCOL_WRITER != 0);
    }
}

pub(super) struct ProtocolWriteGuard;

impl Drop for ProtocolWriteGuard {
    fn drop(&mut self) {
        debug_assert_eq!(PROTOCOL_GATE.load(Ordering::Relaxed), PROTOCOL_WRITER);
        PROTOCOL_GATE.store(0, Ordering::Release);
    }
}

pub(super) fn protocol_read() -> ProtocolReadGuard {
    loop {
        let state = PROTOCOL_GATE.load(Ordering::Acquire);
        if state & PROTOCOL_WRITER != 0 {
            spin_loop();
            continue;
        }
        assert!(
            state < PROTOCOL_WRITER - 1,
            "protocol reader count overflow"
        );
        if PROTOCOL_GATE
            .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return ProtocolReadGuard;
        }
    }
}

pub(super) fn protocol_write() -> ProtocolWriteGuard {
    loop {
        let state = PROTOCOL_GATE.load(Ordering::Acquire);
        if state & PROTOCOL_WRITER != 0
            || PROTOCOL_GATE
                .compare_exchange_weak(
                    state,
                    state | PROTOCOL_WRITER,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            spin_loop();
            continue;
        }
        while PROTOCOL_GATE.load(Ordering::Acquire) != PROTOCOL_WRITER {
            spin_loop();
        }
        return ProtocolWriteGuard;
    }
}
