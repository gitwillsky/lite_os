//! native 调用：编号分派、参数解码、用户内存复制、句柄查找与状态码。
//!
//! 线程的 trap 语义（normal/restricted 模式、异常处理）由 [`owner`] 为 `object::Thread` 实现
//! `ThreadOwner`；各调用按领域分在子模块中，每个处理函数只返回 [`Outcome`]，不直接写寄存器。

mod futex;
mod handles;
mod hardware;
mod ipc;
mod memory;
mod owner;
mod restricted;
mod system;
mod task;

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_abi::{Handle, Status, call};

use crate::object::{HandleEntry, Object, Process};
use crate::vm::AddressSpace;

/// 一次调用的结果。
pub(super) enum Outcome {
    /// 写回状态码并返回调用点。
    Return(Status),
    /// 寄存器已被处理函数整体替换（进入 restricted mode），不写回状态码。
    Preserve,
    /// 调用线程必须退出。
    Exit,
}

impl From<Result<(), Status>> for Outcome {
    fn from(result: Result<(), Status>) -> Self {
        Self::Return(result.err().unwrap_or(native_abi::OK))
    }
}

/// 一次调用的上下文。
pub(super) struct Call<'a> {
    pub(super) thread: &'a Arc<crate::thread::Thread>,
    pub(super) owner: &'a Arc<crate::object::Thread>,
    pub(super) arguments: [usize; 7],
}

/// 可按字节在用户内存与内核之间复制的类型。
///
/// # Safety
///
/// 实现者必须是 `repr(C)`/整数等没有填充以外不变量的类型：任意字节模式都是合法值。
pub(super) unsafe trait Plain: Copy + Default {}

// SAFETY: 整数与 native-abi 中的 repr(C) 结构没有有效性不变量，任意字节都是合法值。
unsafe impl Plain for u32 {}
// SAFETY: 同上。
unsafe impl Plain for u64 {}
// SAFETY: 同上。
unsafe impl Plain for i64 {}
// SAFETY: 同上。
unsafe impl Plain for usize {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::ChannelActual {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::ChannelCallArgs {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::PortPacket {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::register::RegisterState {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::RestrictedState {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::ExceptionReport {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::HandleBasic {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::TaskRuntime {}
// SAFETY: TaskStats 是 repr(C) 的三个 u64，不含 padding、引用或无效位模式。
unsafe impl Plain for native_abi::TaskStats {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::VmarRegion {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::VmoInfo {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::ProcessBasic {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::SystemBasic {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::DebuglogRecord {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::SmmuDescriptor {}
// SAFETY: 同上。
unsafe impl Plain for native_abi::RiscvIommuDescriptor {}

/// 把 `Plain` 值视为字节。
pub(super) fn bytes_of<T: Plain>(value: &T) -> &[u8] {
    // SAFETY: `Plain` 保证按字节读取不违反不变量；切片长度等于类型大小且生命周期绑定到 `value`。
    unsafe {
        core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>())
    }
}

fn bytes_of_mut<T: Plain>(value: &mut T) -> &mut [u8] {
    // SAFETY: `Plain` 保证任意字节写入后仍是合法值。
    unsafe {
        core::slice::from_raw_parts_mut((value as *mut T).cast::<u8>(), core::mem::size_of::<T>())
    }
}

impl Call<'_> {
    pub(super) fn arg(&self, index: usize) -> usize {
        self.arguments[index]
    }

    pub(super) fn arg_u32(&self, index: usize) -> u32 {
        self.arguments[index] as u32
    }

    pub(super) fn arg_handle(&self, index: usize) -> Handle {
        self.arguments[index] as Handle
    }

    pub(super) fn process(&self) -> &Arc<Process> {
        self.owner.process()
    }

    fn space(&self) -> Result<Arc<AddressSpace>, Status> {
        self.thread.address_space().ok_or(native_abi::ERR_BAD_STATE)
    }

    /// 读取用户内存中的一个值。
    pub(super) fn read<T: Plain>(&self, address: usize) -> Result<T, Status> {
        let mut value = T::default();
        self.space()?
            .copy_from_user(address, bytes_of_mut(&mut value))
            .map_err(|_| native_abi::ERR_INVALID_POINTER)?;
        Ok(value)
    }

    /// 写回一个值；`address` 为 0 时忽略（可选输出参数）。
    pub(super) fn write_optional<T: Plain>(&self, address: usize, value: &T) -> Result<(), Status> {
        if address == 0 {
            return Ok(());
        }
        self.write(address, value)
    }

    /// 写回一个值。
    pub(super) fn write<T: Plain>(&self, address: usize, value: &T) -> Result<(), Status> {
        self.space()?
            .copy_to_user(address, bytes_of(value))
            .map_err(|_| native_abi::ERR_INVALID_POINTER)
    }

    /// 读取用户字节到新分配的缓冲区。
    pub(super) fn read_bytes(&self, address: usize, length: usize) -> Result<Vec<u8>, Status> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        bytes.resize(length, 0);
        self.space()?
            .copy_from_user(address, &mut bytes)
            .map_err(|_| native_abi::ERR_INVALID_POINTER)?;
        Ok(bytes)
    }

    /// 读取用户内存中的值数组。
    pub(super) fn read_array<T: Plain>(
        &self,
        address: usize,
        count: usize,
    ) -> Result<Vec<T>, Status> {
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        values.resize(count, T::default());
        if count != 0 {
            // SAFETY: Plain 数组按字节连续，长度为 count * size_of::<T>()。
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(
                    values.as_mut_ptr().cast::<u8>(),
                    count * core::mem::size_of::<T>(),
                )
            };
            self.space()?
                .copy_from_user(address, bytes)
                .map_err(|_| native_abi::ERR_INVALID_POINTER)?;
        }
        Ok(values)
    }

    /// 写回用户字节。
    pub(super) fn write_bytes(&self, address: usize, bytes: &[u8]) -> Result<(), Status> {
        self.space()?
            .copy_to_user(address, bytes)
            .map_err(|_| native_abi::ERR_INVALID_POINTER)
    }

    /// 写回值数组。
    pub(super) fn write_array<T: Plain>(&self, address: usize, values: &[T]) -> Result<(), Status> {
        // SAFETY: Plain 数组按字节连续。
        let bytes = unsafe {
            core::slice::from_raw_parts(
                values.as_ptr().cast::<u8>(),
                core::mem::size_of_val(values),
            )
        };
        self.write_bytes(address, bytes)
    }

    /// 查找句柄并检查 rights。
    pub(super) fn entry(&self, handle: Handle, rights: u32) -> Result<HandleEntry, Status> {
        let entry = self.process().handles().get(handle)?;
        if entry.rights & rights != rights {
            return Err(native_abi::ERR_ACCESS_DENIED);
        }
        Ok(entry)
    }

    /// 把新对象放进调用者句柄表并写回句柄值；写回失败时关闭新句柄。
    pub(super) fn publish(&self, object: Object, output: usize) -> Result<Handle, Status> {
        let handle = self.process().handles().insert(object.into_entry())?;
        if let Err(status) = self.write(output, &handle) {
            drop(self.process().handles().remove(handle));
            return Err(status);
        }
        Ok(handle)
    }
}

/// 从 entry 取出指定类型的对象。
macro_rules! typed {
    ($name:ident, $variant:ident, $type:ty) => {
        impl Call<'_> {
            pub(super) fn $name(&self, handle: Handle, rights: u32) -> Result<Arc<$type>, Status> {
                match self.entry(handle, rights)?.object {
                    Object::$variant(object) => Ok(object),
                    _ => Err(native_abi::ERR_WRONG_TYPE),
                }
            }
        }
    };
}

typed!(job, Job, crate::object::Job);
typed!(process_handle, Process, crate::object::Process);
typed!(thread_handle, Thread, crate::object::Thread);
typed!(vmo, Vmo, crate::vm::Vmo);
typed!(vmar, Vmar, crate::vm::Vmar);
typed!(channel, Channel, crate::object::Channel);
typed!(port, Port, crate::object::Port);
typed!(pager, Pager, crate::object::Pager);
typed!(resource, Resource, crate::object::Resource);
typed!(interrupt, Interrupt, crate::object::Interrupt);
typed!(iommu, Iommu, crate::object::Iommu);
typed!(bti, Bti, crate::object::Bti);
typed!(exception, Exception, crate::object::Exception);

/// 截止时间参数：`DEADLINE_INFINITE` 映射为不超时。
pub(super) fn deadline(raw: usize) -> Option<u64> {
    let raw = raw as i64;
    if raw == native_abi::DEADLINE_INFINITE {
        None
    } else {
        Some(raw.max(0) as u64)
    }
}

/// 根据编号分派一次调用。
pub(super) fn dispatch(call: &Call<'_>, number: usize) -> Outcome {
    match number {
        call::HANDLE_CLOSE => handles::close(call).into(),
        call::HANDLE_DUPLICATE => handles::duplicate(call).into(),
        call::HANDLE_REPLACE => handles::replace(call).into(),
        call::OBJECT_WAIT_ONE => handles::wait_one(call).into(),
        call::OBJECT_WAIT_ASYNC => handles::wait_async(call).into(),
        call::OBJECT_SIGNAL => handles::signal(call).into(),
        call::OBJECT_GET_INFO => handles::get_info(call).into(),
        call::OBJECT_SET_NAME => handles::set_name(call).into(),
        call::CHANNEL_CREATE => ipc::channel_create(call).into(),
        call::CHANNEL_WRITE => ipc::channel_write(call).into(),
        call::CHANNEL_READ => ipc::channel_read(call).into(),
        call::CHANNEL_CALL => ipc::channel_call(call).into(),
        call::EVENT_CREATE => ipc::event_create(call).into(),
        call::PORT_CREATE => ipc::port_create(call).into(),
        call::PORT_WAIT => ipc::port_wait(call).into(),
        call::PORT_QUEUE => ipc::port_queue(call).into(),
        call::VMO_CREATE => memory::vmo_create(call).into(),
        call::VMO_READ => memory::vmo_read(call).into(),
        call::VMO_WRITE => memory::vmo_write(call).into(),
        call::VMO_GET_SIZE => memory::vmo_get_size(call).into(),
        call::VMO_SET_SIZE => memory::vmo_set_size(call).into(),
        call::VMO_OP_RANGE => memory::vmo_op_range(call).into(),
        call::VMO_CREATE_CHILD => memory::vmo_create_child(call).into(),
        call::VMO_CREATE_CONTIGUOUS => memory::vmo_create_contiguous(call).into(),
        call::VMO_CREATE_PHYSICAL => memory::vmo_create_physical(call).into(),
        call::VMAR_MAP => memory::vmar_map(call).into(),
        call::VMAR_UNMAP => memory::vmar_unmap(call).into(),
        call::VMAR_PROTECT => memory::vmar_protect(call).into(),
        call::VMAR_ALLOCATE => memory::vmar_allocate(call).into(),
        call::VMAR_DESTROY => memory::vmar_destroy(call).into(),
        call::PROCESS_READ_MEMORY => memory::process_read_memory(call).into(),
        call::PROCESS_WRITE_MEMORY => memory::process_write_memory(call).into(),
        call::PAGER_CREATE => memory::pager_create(call).into(),
        call::PAGER_CREATE_VMO => memory::pager_create_vmo(call).into(),
        call::PAGER_SUPPLY_PAGES => memory::pager_supply_pages(call).into(),
        call::PAGER_OP => memory::pager_op(call).into(),
        call::JOB_CREATE => task::job_create(call).into(),
        call::PROCESS_CREATE => task::process_create(call).into(),
        call::PROCESS_CREATE_SHARED => task::process_create_shared(call).into(),
        call::PROCESS_START => task::process_start(call).into(),
        call::PROCESS_EXIT => task::process_exit(call),
        call::THREAD_CREATE => task::thread_create(call).into(),
        call::THREAD_START => task::thread_start(call).into(),
        call::THREAD_EXIT => Outcome::Exit,
        call::TASK_KILL => task::task_kill(call),
        call::THREAD_SET_PRIORITY => task::thread_set_priority(call).into(),
        call::TASK_CREATE_EXCEPTION_CHANNEL => task::create_exception_channel(call).into(),
        call::EXCEPTION_RESOLVE => task::exception_resolve(call).into(),
        call::FUTEX_WAIT => futex::wait(call).into(),
        call::FUTEX_WAKE => futex::wake(call).into(),
        call::FUTEX_REQUEUE => futex::requeue(call).into(),
        call::RESTRICTED_BIND_STATE => restricted::bind_state(call).into(),
        call::RESTRICTED_UNBIND_STATE => restricted::unbind_state(call).into(),
        call::RESTRICTED_ENTER => restricted::enter(call),
        call::RESTRICTED_KICK => restricted::kick(call).into(),
        call::RESOURCE_CREATE => hardware::resource_create(call).into(),
        call::INTERRUPT_CREATE => hardware::interrupt_create(call).into(),
        call::INTERRUPT_WAIT => hardware::interrupt_wait(call).into(),
        call::INTERRUPT_ACK => hardware::interrupt_ack(call).into(),
        call::INTERRUPT_BIND => hardware::interrupt_bind(call).into(),
        call::IOMMU_CREATE => hardware::iommu_create(call).into(),
        call::BTI_CREATE => hardware::bti_create(call).into(),
        call::BTI_PIN => hardware::bti_pin(call).into(),
        call::PMT_UNPIN => hardware::pmt_unpin(call).into(),
        call::DEBUGLOG_WRITE => system::debuglog_write(call).into(),
        call::DEBUGLOG_READ => system::debuglog_read(call).into(),
        call::SERIAL_READ => system::serial_read(call).into(),
        call::CLOCK_GET_MONOTONIC => system::clock_get_monotonic(call).into(),
        call::CLOCK_GET_UTC => system::clock_get_utc(call).into(),
        call::CLOCK_SET_UTC => system::clock_set_utc(call).into(),
        call::NANOSLEEP => system::nanosleep(call).into(),
        call::CPRNG_DRAW => system::cprng_draw(call).into(),
        call::CPRNG_ADD_ENTROPY => system::cprng_add_entropy(call).into(),
        call::SYSTEM_GET_INFO => system::get_info(call).into(),
        call::SYSTEM_POWER => system::power(call).into(),
        call::THREAD_SET_AFFINITY => task::thread_set_affinity(call).into(),
        call::CPU_CURRENT => task::cpu_current(call).into(),
        call::SERIAL_WRITE => system::serial_write(call).into(),
        call::SYSTEM_BARRIER => system::barrier().into(),
        _ => Outcome::Return(native_abi::ERR_BAD_SYSCALL),
    }
}
