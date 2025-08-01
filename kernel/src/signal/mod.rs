// Signal System Module
// Unix-like signal system implementation with multi-core support

pub mod signal;
pub mod signal_manager;
pub mod signal_state;
pub mod signal_delivery;
pub mod task_state_manager;
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
    SignalManager, SignalEvent, SignalEventBus, EventCallback,
    SIGNAL_MANAGER, SIGNAL_EVENT_BUS,
    init, process_multicore_signals, get_multicore_signal_stats,
    notify_task_status_change, notify_signal_delivered, notify_task_exited, notify_task_created,
};

pub use signal_state::{
    AtomicSignalState, DEFAULT_BATCH_PROCESSOR,
};

pub use signal_delivery::{
    SafeSignalDelivery, UserStackValidator, SignalDeliveryError,
};

pub use task_state_manager::{
    TaskStateManager, TaskStateTransitionValidator, TaskStatusStats,
    TaskStateEvent, TASK_STATE_MANAGER,
    update_task_status, stop_task, resume_task, terminate_task, get_task_statistics,
};

pub use multicore_signal::{
    MultiCoreSignalManager, InterCoreSignalMessage, CoreSignalStats,
    MULTICORE_SIGNAL_MANAGER,
};