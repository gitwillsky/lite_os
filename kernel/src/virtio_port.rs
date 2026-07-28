//! @description Linux character-device projection for the selected VirtIO Console byte stream.

use alloc::sync::Arc;
use spin::Once;

use crate::{
    drivers::{PortError, VirtIOConsoleDevice},
    ipc::{Pipe, PipeDirection, PipeEnd},
};

/// @description Character-device byte-stream error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    /// The current operation would block.
    WouldBlock,
    /// The named port is closed or the device failed.
    Disconnected,
}

impl From<PortError> for Error {
    fn from(value: PortError) -> Self {
        match value {
            PortError::WouldBlock => Self::WouldBlock,
            PortError::Disconnected => Self::Disconnected,
        }
    }
}

/// @description System-wide projection of one standard VirtIO port.
pub(crate) struct Port {
    device: Arc<VirtIOConsoleDevice>,
    notification_read: Arc<PipeEnd>,
    notification_write: Arc<PipeEnd>,
}

// OWNER: virtio_port retains the only task-aware readiness projection for the physical port.
// Without one publication, separate devfs opens could signal different Pipes and lose wakeups.
static PORT: Once<Arc<Port>> = Once::new();

/// @description Publish the selected adapter and its task-aware readiness source.
/// @param device Platform-owned physical adapter.
/// @param notification Read/write endpoints used only for merged readiness edges.
/// @return The first complete publication succeeds.
pub(crate) fn init(
    device: Arc<VirtIOConsoleDevice>,
    notification: (Arc<PipeEnd>, Arc<PipeEnd>),
) -> Result<(), ()> {
    if PORT.get().is_some() {
        return Err(());
    }
    let port = Arc::try_new(Port {
        device,
        notification_read: notification.0,
        notification_write: notification.1,
    })
    .map_err(|_| ())?;
    PORT.call_once(|| port);
    Ok(())
}

/// @description Open the system VirtIO port character backend.
/// @return A shared byte-stream handle, or `None` when this platform has no port.
pub(crate) fn open() -> Option<Arc<Port>> {
    PORT.get().cloned()
}

impl Port {
    /// @description Consume available device bytes without sleeping.
    /// @param output Kernel-owned destination.
    /// @return Byte count or a precise readiness/device error.
    pub(crate) fn read(&self, output: &mut [u8]) -> Result<usize, Error> {
        self.device.read(output).map_err(Into::into)
    }

    /// @description Submit a bounded byte fragment without sleeping.
    /// @param input Kernel-owned source.
    /// @return Submitted byte count or a precise readiness/device error.
    pub(crate) fn write(&self, input: &[u8]) -> Result<usize, Error> {
        self.device.write(input).map_err(Into::into)
    }

    pub(crate) fn poll_events(&self, events: i16) -> i16 {
        const INPUT: i16 = 0x001;
        const OUTPUT: i16 = 0x004;
        const ERROR: i16 = 0x008;
        const HANGUP: i16 = 0x010;
        let mut ready = 0;
        if !self.device.connected() {
            return ERROR | HANGUP;
        }
        if self.device.readable() {
            ready |= events & INPUT;
        }
        if self.device.writable() {
            ready |= events & OUTPUT;
        }
        ready
    }

    pub(crate) fn readiness_generation(&self) -> u64 {
        self.notification_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    pub(crate) fn notification_pipe(&self) -> Arc<Pipe> {
        self.notification_read.pipe()
    }

    pub(crate) fn prepare_to_block(&self, events: i16) -> Option<Arc<Pipe>> {
        if self.poll_events(events) != 0 {
            return None;
        }
        self.notification_read.drain_readiness();
        (self.poll_events(events) == 0).then(|| self.notification_read.pipe())
    }
}

/// @description Drain adapter completions and publish one merged task readiness edge.
/// @return `true` when a bounded pass left queue backlog.
pub(crate) fn dispatch_work() -> bool {
    let Some(port) = PORT.get() else {
        return false;
    };
    let activity = port.device.dispatch();
    if activity.readable_changed || activity.writable_changed {
        port.notification_write.signal_readiness();
    }
    activity.backlog
}
