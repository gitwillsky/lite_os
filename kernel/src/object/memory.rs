//! 内存对象的句柄语义：VMO、VMAR 与 pager。

use alloc::sync::{Arc, Weak};

use native_abi::{ObjectType, PortPacket, Status, rights};

use super::{KernelObject, Port, next_koid};
use crate::vm::{PagerRequest, PagerSink, VmError, Vmar, Vmo};

/// `VmError` 到 native status 的唯一映射。
pub(crate) fn vm_status(error: VmError) -> Status {
    match error {
        VmError::NoMemory => native_abi::ERR_NO_MEMORY,
        VmError::InvalidArgs => native_abi::ERR_INVALID_ARGS,
        VmError::OutOfRange => native_abi::ERR_OUT_OF_RANGE,
        VmError::AddressInUse => native_abi::ERR_INVALID_ARGS,
        VmError::NotMapped => native_abi::ERR_NOT_FOUND,
        VmError::AccessDenied => native_abi::ERR_ACCESS_DENIED,
        VmError::BadState => native_abi::ERR_BAD_STATE,
        VmError::NotSupported => native_abi::ERR_NOT_SUPPORTED,
        VmError::Io => native_abi::ERR_IO,
        VmError::Interrupted => native_abi::ERR_CANCELED,
    }
}

impl KernelObject for Vmo {
    fn object_type(&self) -> ObjectType {
        ObjectType::Vmo
    }
    fn koid(&self) -> u64 {
        self.id()
    }
    fn default_rights(&self) -> u32 {
        rights::DUPLICATE
            | rights::TRANSFER
            | rights::READ
            | rights::WRITE
            | rights::EXECUTE
            | rights::MAP
            | rights::INSPECT
    }
}

impl KernelObject for Vmar {
    fn object_type(&self) -> ObjectType {
        ObjectType::Vmar
    }
    fn koid(&self) -> u64 {
        self.koid()
    }
    fn default_rights(&self) -> u32 {
        rights::DUPLICATE
            | rights::TRANSFER
            | rights::READ
            | rights::WRITE
            | rights::EXECUTE
            | rights::MAP
            | rights::MANAGE
            | rights::INSPECT
    }
}

/// pager：其创建的 VMO 缺页时向绑定的 port 投递 `packet::PAGE_REQUEST`。
pub(crate) struct Pager {
    koid: u64,
}

impl Pager {
    /// 创建 pager。
    pub(crate) fn new() -> Self {
        Self { koid: next_koid() }
    }

    /// 创建由本 pager 支撑的 VMO。
    ///
    /// # Errors
    /// 分配失败返回 `ERR_NO_MEMORY`。
    pub(crate) fn create_vmo(
        &self,
        port: &Arc<Port>,
        key: u64,
        size: u64,
        resizable: bool,
        anonymous: bool,
    ) -> Result<Arc<Vmo>, Status> {
        let sink = Arc::try_new(PortSink {
            port: Arc::downgrade(port),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        Vmo::create_paged(
            size,
            crate::vm::VmoOptions { resizable },
            crate::vm::PagerBinding { sink, key },
            anonymous,
        )
        .map_err(vm_status)
    }
}

impl KernelObject for Pager {
    fn object_type(&self) -> ObjectType {
        ObjectType::Pager
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        rights::DUPLICATE | rights::TRANSFER | rights::INSPECT | rights::MANAGE
    }
}

/// 把 pager 请求投递到 port；port 销毁后请求失败，等待者以 `ERR_IO` 结束。
struct PortSink {
    port: Weak<Port>,
}

impl PagerSink for PortSink {
    fn request(&self, request: PagerRequest) -> Result<(), ()> {
        let port = self.port.upgrade().ok_or(())?;
        port.queue(PortPacket {
            key: request.key,
            kind: native_abi::packet::PAGE_REQUEST,
            status: native_abi::OK,
            trigger: 0,
            observed: (request.length / native_abi::PAGE_SIZE as u64) as u32,
            payload: request.offset,
        })
        .map_err(|_| ())
    }
}
