//! seccomp-focused host seams: actual Pipe/ID producers; OFD payload and user-copy routing adapters.
//! No filter, request, notification state, cancellation or injection algorithm is implemented here.
#[path = "../../../kernel/src/id.rs"]
pub(crate) mod id;
#[path = "pipe_fixture.rs"]
pub(crate) mod ipc;

pub(crate) mod file {
    use alloc::sync::Arc;
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum FileDescriptorError {
        NotFound,
        Limit,
        OutOfMemory,
        Busy,
    }
    pub(crate) struct OpenFileDescription {
        pub(crate) identity: usize,
        pub(crate) device: Option<Arc<dyn super::chardev::DeviceFile>>,
    }
    pub(crate) struct PreparedDeviceDescription;
    impl PreparedDeviceDescription {
        pub(crate) fn try_new(_: &[u8], _: u32) -> Result<Self, crate::fs::FileSystemError> {
            Ok(Self)
        }
        pub(crate) fn finish(
            self,
            file: Arc<dyn super::chardev::DeviceFile>,
        ) -> Arc<OpenFileDescription> {
            Arc::new(OpenFileDescription {
                identity: 0,
                device: Some(file),
            })
        }
    }
    pub(crate) struct Reservation;
    impl Reservation {
        pub(crate) fn publish(self, _: Arc<OpenFileDescription>) -> usize {
            unreachable!("policy-only fixture never publishes listener descriptors")
        }
    }
}
pub(crate) mod fs {
    pub(crate) use super::chardev;
    pub(crate) const O_RDWR: u32 = 2;
    pub(crate) const O_CLOEXEC: u32 = 0o2000000;
    #[derive(Debug)]
    pub(crate) enum FileSystemError {
        OutOfMemory,
    }
}
pub(crate) mod chardev {
    use super::ipc::Pipe;
    use alloc::sync::Arc;
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum DeviceError {
        Errno(isize),
        Restart,
    }
    #[derive(Debug)]
    pub(crate) struct UserFault;
    pub(crate) trait UserMemory {
        fn pid_namespace(&self) -> Arc<crate::namespace::PidNamespace>;
        fn read(&self, address: usize, bytes: &mut [u8]) -> Result<(), UserFault>;
        fn write(&self, address: usize, bytes: &[u8]) -> Result<(), UserFault>;
    }
    pub(crate) struct IoctlCall<'a> {
        pub(crate) request: usize,
        pub(crate) argument: usize,
        pub(crate) user: &'a dyn UserMemory,
        pub(crate) fd_readable: bool,
        pub(crate) nonblocking: bool,
    }
    pub(crate) struct DeviceWaitSources {
        pub(crate) pipe: Arc<Pipe>,
        pub(crate) events: i16,
    }
    impl DeviceWaitSources {
        pub(crate) fn pipe(pipe: Arc<Pipe>, events: i16) -> Self {
            Self { pipe, events }
        }
    }
    pub(crate) trait DeviceFile: Send + Sync {
        fn supports_read(&self) -> bool {
            true
        }
        fn supports_write(&self) -> bool {
            true
        }
        fn release(&self) {}
        fn anonymous_name(&self) -> &'static [u8];
        fn poll(&self, events: i16) -> i16;
        fn wait_sources(&self, events: i16) -> DeviceWaitSources;
        fn readiness_generation(&self) -> u64;
        fn prepare_wait(&self, events: i16) -> Option<Arc<Pipe>>;
        fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError>;
    }
    // Scheduler adapter: real Pipe wait; production chardev::wait_ready errno routing is covered in guest.
    pub(crate) fn wait_ready(
        file: &dyn DeviceFile,
        events: i16,
        _: bool,
    ) -> Result<(), DeviceError> {
        if let Some(pipe) = file.prepare_wait(events) {
            match pipe.wait(super::ipc::PipeWaitCondition::Readable, None) {
                crate::execution::sync::WaitResult::Woken => {}
                crate::execution::sync::WaitResult::Interrupted => {
                    return Err(DeviceError::Errno(syscall_abi::errno::EINTR));
                }
                crate::execution::sync::WaitResult::OutOfMemory => {
                    return Err(DeviceError::Errno(syscall_abi::errno::ENOMEM));
                }
                other => panic!("unexpected host notification wait: {other:?}"),
            }
        }
        Ok(())
    }
}
