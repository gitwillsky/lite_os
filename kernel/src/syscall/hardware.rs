//! resource、中断、IOMMU 与 BTI 调用。

use alloc::sync::Arc;

use native_abi::{Status, rights};

use super::Call;
use crate::object::{Bti, Interrupt, Iommu, Msi, Object};

pub(super) fn resource_create(call: &Call<'_>) -> Result<(), Status> {
    let parent = call.resource(call.arg_handle(0), rights::WRITE)?;
    let child = parent.create_child(call.arg_u32(1), call.arg(2) as u64, call.arg(3) as u64)?;
    let child = Arc::try_new(child).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    call.publish(Object::Resource(child), call.arg(4))
        .map(|_| ())
}

pub(super) fn interrupt_create(call: &Call<'_>) -> Result<(), Status> {
    let resource = call.resource(call.arg_handle(0), 0)?;
    let vector = call.arg_u32(1);
    if !resource.covers_irq(vector) {
        return Err(native_abi::ERR_ACCESS_DENIED);
    }
    let options = call.arg_u32(2);
    if options & !native_abi::interrupt::LEVEL != 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let interrupt = Interrupt::create(vector, options & native_abi::interrupt::LEVEL != 0)?;
    let interrupt = Arc::try_new(interrupt).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    call.publish(Object::Interrupt(interrupt), call.arg(3))
        .map(|_| ())
}

pub(super) fn interrupt_wait(call: &Call<'_>) -> Result<(), Status> {
    let interrupt = call.interrupt(call.arg_handle(0), rights::WAIT)?;
    let timestamp = interrupt.wait(super::deadline(call.arg(1)))?;
    call.write_optional(call.arg(2), &timestamp)
}

pub(super) fn interrupt_ack(call: &Call<'_>) -> Result<(), Status> {
    call.interrupt(call.arg_handle(0), rights::WRITE)?.ack()
}

pub(super) fn interrupt_bind(call: &Call<'_>) -> Result<(), Status> {
    let interrupt = call.interrupt(call.arg_handle(0), rights::READ)?;
    let port = call.port(call.arg_handle(1), rights::WRITE)?;
    interrupt.bind_port(port, call.arg(2) as u64)
}

pub(super) fn interrupt_set_affinity(call: &Call<'_>) -> Result<(), Status> {
    let interrupt = call.interrupt(call.arg_handle(0), rights::WRITE)?;
    let cpu = crate::execution::cpu::id_at(call.arg(1)).ok_or(native_abi::ERR_INVALID_ARGS)?;
    interrupt.set_affinity(cpu)
}

pub(super) fn iommu_create(call: &Call<'_>) -> Result<(), Status> {
    let resource = call.resource(call.arg_handle(0), 0)?;
    if !resource.is_root() {
        return Err(native_abi::ERR_ACCESS_DENIED);
    }
    let length = call.arg(3);
    if length > 256 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let descriptor = call.read_bytes(call.arg(2), length)?;
    let iommu = Iommu::create(call.arg_u32(1), &descriptor)?;
    let iommu = Arc::try_new(iommu).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    call.publish(Object::Iommu(iommu), call.arg(4)).map(|_| ())
}

pub(super) fn bti_create(call: &Call<'_>) -> Result<(), Status> {
    let iommu = call.iommu(call.arg_handle(0), 0)?;
    let bti = Arc::try_new(Bti::create(&iommu, call.arg_u32(1)))
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    call.publish(Object::Bti(bti), call.arg(2)).map(|_| ())
}

pub(super) fn bti_pin(call: &Call<'_>) -> Result<(), Status> {
    let options = call.arg_u32(1);
    if options & !(native_abi::bti::READ | native_abi::bti::WRITE) != 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let writable = options & native_abi::bti::WRITE != 0;
    let bti = call.bti(
        call.arg_handle(0),
        rights::READ | if writable { rights::WRITE } else { 0 },
    )?;
    let vmo_rights = rights::MAP
        | if writable {
            rights::WRITE
        } else {
            rights::READ
        };
    let vmo = call.vmo(call.arg_handle(2), vmo_rights)?;
    let pmt = bti.pin(&vmo, call.arg(3) as u64, call.arg(4) as u64, writable)?;
    let addresses = pmt.addresses();
    call.write_array(call.arg(5), addresses)?;
    let pmt = Arc::try_new(pmt).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    call.publish(Object::Pmt(pmt), call.arg(6)).map(|_| ())
}

pub(super) fn pmt_unpin(call: &Call<'_>) -> Result<(), Status> {
    let handles = call.process().handles();
    let handle = call.arg_handle(0);
    let entry = handles.get(handle)?;
    let Object::Pmt(pmt) = &entry.object else {
        return Err(native_abi::ERR_WRONG_TYPE);
    };
    // PMT 不可复制，句柄是唯一引用者：标记后关闭即释放，不进入隔离区。
    pmt.mark_unpinned();
    drop(handles.remove(handle)?);
    Ok(())
}

pub(super) fn bti_release_quarantine(call: &Call<'_>) -> Result<(), Status> {
    call.bti(call.arg_handle(0), rights::WRITE)?
        .release_quarantine();
    Ok(())
}

/// MSI resource 与 BTI 必须来自同一设备的受信装配；内核只验证不可扩大 authority。
pub(super) fn msi_allocate(call: &Call<'_>) -> Result<(), Status> {
    let resource = call.resource(call.arg_handle(0), rights::WRITE)?;
    let bti = call.bti(call.arg_handle(1), rights::READ | rights::WRITE)?;
    let device = call.arg_u32(2);
    if !resource.covers_msi(device) {
        return Err(native_abi::ERR_ACCESS_DENIED);
    }
    let group = Arc::try_new(Msi::create(bti, device, call.arg_u32(3))?)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    call.publish(Object::Msi(group), call.arg(4)).map(|_| ())
}
pub(super) fn msi_create_interrupt(call: &Call<'_>) -> Result<(), Status> {
    let group = call.msi(call.arg_handle(0), rights::WRITE)?;
    let table = call.vmo(
        call.arg_handle(2),
        rights::READ | rights::WRITE | rights::MAP,
    )?;
    let interrupt = Arc::try_new(group.interrupt(call.arg_u32(1), &table, call.arg(3))?)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    call.publish(Object::Interrupt(interrupt), call.arg(4))
        .map(|_| ())
}
