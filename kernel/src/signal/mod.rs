// Signal System Module
// Unix-like signal system implementation with multi-core support

pub mod signal;
pub mod signal_manager;
pub mod signal_state;
pub mod signal_delivery;
pub mod multicore_signal;

// Re-export commonly used types and functions
pub use signal::{
    Signal, SignalSet, SignalAction, SignalDisposition, SignalState,
    SignalError, SignalFrame, SignalDelivery,
    SIG_BLOCK, SIG_UNBLOCK, SIG_SETMASK,
    SA_NOCLDSTOP, SA_NOCLDWAIT, SA_SIGINFO, SA_RESTART, SA_NODEFER, SA_RESETHAND, SA_ONSTACK,
    SIG_DFL, SIG_IGN, SIG_RETURN_ADDR,
    uncatchable_signals, stop_signals,
    send_signal_to_process, check_and_handle_signals, check_and_handle_signals_with_cx,
};

pub use signal_manager::{
    SignalManager, SIGNAL_MANAGER,
    init, process_multicore_signals, get_multicore_signal_stats,
};

pub use signal_state::{
    AtomicSignalState, DEFAULT_BATCH_PROCESSOR,
};

pub use signal_delivery::{
    SafeSignalDelivery, UserStackValidator, SignalDeliveryError,
};


pub use multicore_signal::{
    MultiCoreSignalManager, InterCoreSignalMessage, CoreSignalStats,
    MULTICORE_SIGNAL_MANAGER,
};