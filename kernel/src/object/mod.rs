//! 内核对象：句柄表、rights、signal 与全部对象类型，经 `native-abi` 调用访问。
//!
//! 对象的公共行为（类型、默认 rights、signal、等待取消）统一由 [`KernelObject`] 描述；
//! 句柄 payload [`Object`] 是封闭 enum，只用于取得类型化引用。

mod channel;
mod event;
mod exception;
mod handle_table;
mod memory;
mod port;
mod resource;
mod signals;
mod task;

use alloc::sync::Arc;

use native_abi::{ObjectType, rights};

pub(crate) use channel::{Channel, Message, ReadError};
pub(crate) use event::{Event, memory_pressure_event, notify_memory_pressure};
pub(crate) use exception::{Exception, ExceptionResolution};
pub(crate) use handle_table::{HandleEntry, HandleTable};
pub(crate) use memory::{Pager, vm_status};
pub(crate) use port::Port;
pub(crate) use resource::{Bti, Interrupt, Iommu, Msi, Pmt, Resource};
pub(crate) use signals::{CancelKey, SignalState, WaitOutcome};
pub(crate) use task::{Job, Mode, Process, ProcessKind, RestrictedEntry, Thread};

pub(crate) use crate::id::next_koid;

/// 所有内核对象的公共行为；句柄上的通用操作只经此 trait 分派。
pub(crate) trait KernelObject: Send + Sync {
    /// 对象类型。
    fn object_type(&self) -> ObjectType;

    /// 对象全局编号。
    fn koid(&self) -> u64;

    /// 新建句柄的初始 rights。
    fn default_rights(&self) -> u32;

    /// 可被 `object_wait_one`/`object_wait_async` 观察的 signal；不可观察的对象返回 `None`。
    fn signals(&self) -> Option<&SignalState> {
        None
    }

    /// 取消经 `key` 登记的全部等待（句柄关闭时调用）。
    fn cancel(&self, key: CancelKey) {
        if let Some(signals) = self.signals() {
            signals.cancel(key);
        }
    }

    /// 用户可通过 `object_signal` 修改的 signal 位。
    fn user_signal_mask(&self) -> u32 {
        native_abi::signals::USER_ALL
    }
}

/// 可等待对象共有的初始 rights。
pub(crate) const WAITABLE_RIGHTS: u32 =
    rights::DUPLICATE | rights::TRANSFER | rights::WAIT | rights::INSPECT;

/// 句柄引用的对象。
#[derive(Clone)]
pub(crate) enum Object {
    Job(Arc<Job>),
    Process(Arc<Process>),
    Thread(Arc<Thread>),
    Vmo(Arc<crate::vm::Vmo>),
    Vmar(Arc<crate::vm::Vmar>),
    Channel(Arc<Channel>),
    Event(Arc<Event>),
    Port(Arc<Port>),
    Pager(Arc<Pager>),
    Resource(Arc<Resource>),
    Interrupt(Arc<Interrupt>),
    Msi(Arc<Msi>),
    Iommu(Arc<Iommu>),
    Bti(Arc<Bti>),
    Pmt(Arc<Pmt>),
    Exception(Arc<Exception>),
}

impl Object {
    /// 对象的公共行为视图。
    pub(crate) fn kernel_object(&self) -> &dyn KernelObject {
        match self {
            Self::Job(object) => object.as_ref(),
            Self::Process(object) => object.as_ref(),
            Self::Thread(object) => object.as_ref(),
            Self::Vmo(object) => object.as_ref(),
            Self::Vmar(object) => object.as_ref(),
            Self::Channel(object) => object.as_ref(),
            Self::Event(object) => object.as_ref(),
            Self::Port(object) => object.as_ref(),
            Self::Pager(object) => object.as_ref(),
            Self::Resource(object) => object.as_ref(),
            Self::Interrupt(object) => object.as_ref(),
            Self::Msi(object) => object.as_ref(),
            Self::Iommu(object) => object.as_ref(),
            Self::Bti(object) => object.as_ref(),
            Self::Pmt(object) => object.as_ref(),
            Self::Exception(object) => object.as_ref(),
        }
    }

    /// 以默认 rights 构造句柄 entry。
    pub(crate) fn into_entry(self) -> HandleEntry {
        let rights = self.kernel_object().default_rights();
        HandleEntry {
            object: self,
            rights,
        }
    }
}
