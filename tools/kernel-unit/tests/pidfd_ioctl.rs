#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际 PIDFS binding/codec 与真实 PID ladder；Task 查询和 user-copy 是显式窄 seam。
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
use alloc::sync::Arc;
use std::cell::RefCell;

#[path = "../../../kernel/src/storage/fs"]
mod fs {
    pub(crate) mod device {
        #[derive(Debug, PartialEq, Eq)]
        pub(crate) enum DeviceError {
            Errno(isize),
        }
        pub(crate) struct IoctlCall<'a> {
            pub(crate) request: usize,
            pub(crate) argument: usize,
            pub(crate) user: &'a dyn UserMemory,
        }
        pub(crate) trait UserMemory {
            fn pid_namespace(&self) -> alloc::sync::Arc<crate::namespace::PidNamespace>;
            fn read(&self, address: usize, bytes: &mut [u8]) -> Result<(), ()>;
            fn write(&self, address: usize, bytes: &[u8]) -> Result<(), ()>;
            fn install_namespace(
                &self,
                namespace: crate::namespace::NamespaceReference,
            ) -> Result<isize, DeviceError>;
        }
    }
    #[path = "pidfs"]
    pub(crate) mod pidfs {
        // 仅提供历史 attr 查询输入；真实 source publication/retirement 由生命周期 guest 覆盖。
        pub(crate) struct PidFdSource(pub(crate) Option<i32>);
        impl PidFdSource {
            pub(crate) fn exit_status(&self) -> Option<i32> {
                self.0
            }
        }
        mod binding;
        pub(crate) use binding::{PidFdPoll, PidFdTarget};
        mod ioctl;
        pub(crate) fn invoke(
            target: &dyn PidFdTarget,
            call: &super::device::IoctlCall<'_>,
        ) -> Result<isize, super::device::DeviceError> {
            ioctl::ioctl(target, call)
        }
    }
}
use fs::{
    device::{DeviceError, IoctlCall, UserMemory},
    pidfs::{PidFdPoll, PidFdSource, PidFdTarget},
};
struct Memory {
    bytes: RefCell<[u8; 128]>,
    accesses: RefCell<Vec<(bool, usize, usize)>>,
    namespace: Arc<namespace::PidNamespace>,
}
impl Memory {
    fn new(mask: u64) -> Self {
        let mut bytes = [0xa5; 128];
        bytes[..8].copy_from_slice(&mask.to_ne_bytes());
        Self {
            bytes: RefCell::new(bytes),
            accesses: RefCell::new(Vec::new()),
            namespace: namespace::initial_pid(),
        }
    }
    fn mask(&self) -> u64 {
        u64::from_ne_bytes(self.bytes.borrow()[..8].try_into().unwrap())
    }
}
impl UserMemory for Memory {
    fn pid_namespace(&self) -> Arc<namespace::PidNamespace> {
        self.namespace.clone()
    }
    fn read(&self, address: usize, bytes: &mut [u8]) -> Result<(), ()> {
        self.accesses
            .borrow_mut()
            .push((false, address, bytes.len()));
        let offset = address.checked_sub(4096).ok_or(())?;
        bytes.copy_from_slice(
            self.bytes
                .borrow()
                .get(offset..offset.checked_add(bytes.len()).ok_or(())?)
                .ok_or(())?,
        );
        Ok(())
    }
    fn write(&self, address: usize, bytes: &[u8]) -> Result<(), ()> {
        self.accesses
            .borrow_mut()
            .push((true, address, bytes.len()));
        let offset = address.checked_sub(4096).ok_or(())?;
        self.bytes
            .borrow_mut()
            .get_mut(offset..offset.checked_add(bytes.len()).ok_or(())?)
            .ok_or(())?
            .copy_from_slice(bytes);
        Ok(())
    }
    fn install_namespace(&self, _: namespace::NamespaceReference) -> Result<isize, DeviceError> {
        Ok(71)
    }
}
// Task 查询返回值是显式 fixture 输入；调用记录只用于证明 codec 转交原生参数。
struct Target {
    identity: Arc<namespace::PidIdentity>,
    source: Arc<PidFdSource>,
    identifiers: Option<[u32; 11]>,
    namespace_calls: spin::Mutex<Vec<(Option<namespace::NamespaceSelector>, bool)>>,
}
impl Target {
    fn new(key: usize, exit: Option<i32>, identifiers: Option<[u32; 11]>) -> Self {
        namespace::boot();
        let reservation = namespace::PreparedPid::try_new(
            key,
            namespace::initial_pid(),
            &[],
            &namespace::root_authority(),
        )
        .unwrap();
        Self {
            identity: reservation.identity(),
            source: Arc::new(PidFdSource(exit)),
            identifiers,
            namespace_calls: spin::Mutex::new(Vec::new()),
        }
    }
}
impl PidFdTarget for Target {
    fn identity(&self) -> &Arc<namespace::PidIdentity> {
        &self.identity
    }
    fn source(&self) -> &Arc<PidFdSource> {
        &self.source
    }
    fn poll(&self) -> PidFdPoll {
        unreachable!("ioctl does not infer task phase from historical attr")
    }
    fn information(&self, _: &namespace::PidNamespace) -> Option<[u32; 11]> {
        self.identifiers
    }
    fn namespace(
        &self,
        selector: Option<namespace::NamespaceSelector>,
        has_argument: bool,
    ) -> Result<namespace::NamespaceReference, DeviceError> {
        self.namespace_calls.lock().push((selector, has_argument));
        Ok(namespace::NamespaceReference::new(namespace::initial_user()))
    }
}
fn info(target: &Target, memory: &Memory, size: usize) -> Result<isize, DeviceError> {
    fs::pidfs::invoke(
        target,
        &IoctlCall {
            request: 0xc000_ff0b | size << 16,
            argument: 4096,
            user: memory,
        },
    )
}
#[test]
fn actual_info_codec_reads_only_input_mask_and_clears_future_tail() {
    let target = Target::new(78001, None, Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]));
    let memory = Memory::new(32);
    assert_eq!(info(&target, &memory, 96), Ok(0));
    assert_eq!(memory.mask(), 1 | 2 | 32);
    assert_eq!(&memory.bytes.borrow()[16..20], &1u32.to_ne_bytes());
    assert_eq!(&memory.bytes.borrow()[56..60], &11u32.to_ne_bytes());
    assert_eq!(&memory.bytes.borrow()[88..96], &[0; 8]);
    assert!(memory.bytes.borrow()[96..].iter().all(|byte| *byte == 0xa5));
    assert_eq!(
        *memory.accesses.borrow(),
        [(false, 4096, 8), (true, 4096, 88), (true, 4184, 8)]
    );
}
#[test]
fn actual_short_structure_does_not_advertise_fields_beyond_its_output() {
    let target = Target::new(78002, Some(43 << 8), Some([0; 11]));
    let memory = Memory::new(8 | 32);
    assert_eq!(info(&target, &memory, 64), Ok(0));
    assert_eq!(memory.mask(), 1 | 2 | 8);
    assert_eq!(&memory.bytes.borrow()[60..64], &(43i32 << 8).to_ne_bytes());
    assert!(memory.bytes.borrow()[64..].iter().all(|byte| *byte == 0xa5));
}
#[test]
fn actual_retired_pid_info_requires_exit_request_and_keeps_native_empty_mask() {
    let target = Target::new(78003, None, None);
    let memory = Memory::new(0);
    assert_eq!(
        info(&target, &memory, 88),
        Err(DeviceError::Errno(syscall_abi::errno::ESRCH))
    );
    let memory = Memory::new(8);
    assert_eq!(info(&target, &memory, 88), Ok(0));
    assert_eq!(memory.mask(), 0);
    let target = Target::new(78004, Some(9), None);
    let memory = Memory::new(8);
    assert_eq!(info(&target, &memory, 88), Ok(0));
    assert_eq!(memory.mask(), 8);
    assert_eq!(&memory.bytes.borrow()[60..64], &9i32.to_ne_bytes());
}
#[test]
fn actual_namespace_codec_forwards_selector_and_argument_presence() {
    let net = Target::new(78006, None, None);
    let memory = Memory::new(0);
    assert_eq!(
        fs::pidfs::invoke(
            &net,
            &IoctlCall {
                request: 0xff04,
                argument: 0,
                user: &memory
            }
        ),
        Ok(71)
    );
    assert_eq!(
        *net.namespace_calls.lock(),
        [(
            Some(namespace::NamespaceSelector::Namespace(
                namespace::NamespaceType::Net
            )),
            false
        )]
    );

    let target = Target::new(78005, None, None);
    let memory = Memory::new(0);
    assert_eq!(
        fs::pidfs::invoke(
            &target,
            &IoctlCall {
                request: 0xff09,
                argument: 1,
                user: &memory
            }
        ),
        Ok(71)
    );
    assert_eq!(
        *target.namespace_calls.lock(),
        [(
            Some(namespace::NamespaceSelector::Namespace(
                namespace::NamespaceType::User
            )),
            true
        )]
    );
    assert_eq!(
        fs::pidfs::invoke(
            &target,
            &IoctlCall {
                request: 0xff0b,
                argument: 0,
                user: &memory
            }
        ),
        Err(DeviceError::Errno(syscall_abi::errno::ENOTTY))
    );
}
