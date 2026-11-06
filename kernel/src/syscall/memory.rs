//! VMO、VMAR、pager 与跨进程内存调用。

use alloc::sync::Arc;

use native_abi::{Status, rights, vm, vmo};

use super::Call;
use crate::object::{Object, Pager, vm_status};
use crate::vm::page_table::PagePermissions;
use crate::vm::{CachePolicy, ChildKind, MapOptions, RangeOp, Vmo, VmoOptions};

/// VMO 一次读写的最大字节数；更大的请求按块分次执行，避免一次分配过大的内核缓冲。
const CHUNK: usize = 64 * 1024;

pub(super) fn vmo_create(call: &Call<'_>) -> Result<(), Status> {
    let options = call.arg_u32(1);
    if options & !vmo::RESIZABLE != 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let vmo = Vmo::create(
        call.arg(0) as u64,
        VmoOptions {
            resizable: options & vmo::RESIZABLE != 0,
        },
    )
    .map_err(vm_status)?;
    call.publish(Object::Vmo(vmo), call.arg(2)).map(|_| ())
}

fn transfer(call: &Call<'_>, write: bool) -> Result<(), Status> {
    let options = call.arg_u32(4);
    if options & !vmo::READ_DONT_WAIT != 0 || write && options != 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let vmo = call.vmo(
        call.arg_handle(0),
        if write { rights::WRITE } else { rights::READ },
    )?;
    let buffer = call.arg(1);
    let offset = call.arg(2) as u64;
    let length = call.arg(3);
    let mut done = 0;
    while done < length {
        let chunk = (length - done).min(CHUNK);
        let at = offset
            .checked_add(done as u64)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        if write {
            let bytes = call.read_bytes(buffer + done, chunk)?;
            vmo.write(at, &bytes).map_err(vm_status)?;
        } else {
            let mut bytes = alloc::vec::Vec::new();
            bytes
                .try_reserve_exact(chunk)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            bytes.resize(chunk, 0);
            if options & vmo::READ_DONT_WAIT != 0 {
                if !vmo.read_available(at, &mut bytes).map_err(vm_status)? {
                    return Err(native_abi::ERR_SHOULD_WAIT);
                }
            } else {
                vmo.read(at, &mut bytes).map_err(vm_status)?;
            }
            call.write_bytes(buffer + done, &bytes)?;
        }
        done += chunk;
    }
    Ok(())
}

pub(super) fn vmo_read(call: &Call<'_>) -> Result<(), Status> {
    transfer(call, false)
}

pub(super) fn vmo_write(call: &Call<'_>) -> Result<(), Status> {
    transfer(call, true)
}

pub(super) fn vmo_get_size(call: &Call<'_>) -> Result<(), Status> {
    let vmo = call.vmo(call.arg_handle(0), rights::INSPECT)?;
    call.write(call.arg(1), &vmo.size())
}

pub(super) fn vmo_set_size(call: &Call<'_>) -> Result<(), Status> {
    let vmo = call.vmo(call.arg_handle(0), rights::WRITE)?;
    vmo.set_size(call.arg(1) as u64).map_err(vm_status)
}

pub(super) fn vmo_op_range(call: &Call<'_>) -> Result<(), Status> {
    let vmo = call.vmo(call.arg_handle(0), rights::WRITE)?;
    let op = match call.arg_u32(1) {
        vmo::OP_COMMIT => RangeOp::Commit,
        vmo::OP_DECOMMIT => RangeOp::Decommit,
        vmo::OP_ZERO => RangeOp::Zero,
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    };
    vmo.op_range(op, call.arg(2) as u64, call.arg(3) as u64)
        .map_err(vm_status)
}

pub(super) fn vmo_create_child(call: &Call<'_>) -> Result<(), Status> {
    let entry = call.entry(call.arg_handle(0), rights::DUPLICATE | rights::READ)?;
    let Object::Vmo(parent) = entry.object else {
        return Err(native_abi::ERR_WRONG_TYPE);
    };
    let kind = match call.arg_u32(1) {
        vmo::CHILD_SNAPSHOT => ChildKind::Snapshot,
        vmo::CHILD_SNAPSHOT_AT_LEAST_ON_WRITE => ChildKind::AtLeastOnWrite,
        vmo::CHILD_SLICE => ChildKind::Slice,
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    };
    let child = parent
        .create_child(call.arg(2) as u64, call.arg(3) as u64, kind)
        .map_err(vm_status)?;
    // COW 子对象内容属于子对象自身，可写；slice 只能继承父句柄的写权限。
    let mut child_rights = entry.rights | rights::WRITE | rights::READ | rights::MAP;
    if kind == ChildKind::Slice && entry.rights & rights::WRITE == 0 {
        child_rights &= !rights::WRITE;
    }
    let handle = call
        .process()
        .handles()
        .insert(crate::object::HandleEntry {
            object: Object::Vmo(child),
            rights: child_rights,
        })?;
    if let Err(status) = call.write(call.arg(4), &handle) {
        drop(call.process().handles().remove(handle));
        return Err(status);
    }
    Ok(())
}

pub(super) fn vmo_create_contiguous(call: &Call<'_>) -> Result<(), Status> {
    let _bti = call.bti(call.arg_handle(0), rights::READ)?;
    let vmo = Vmo::create_contiguous(call.arg(1) as u64).map_err(vm_status)?;
    call.publish(Object::Vmo(vmo), call.arg(2)).map(|_| ())
}

pub(super) fn vmo_create_physical(call: &Call<'_>) -> Result<(), Status> {
    let resource = call.resource(call.arg_handle(0), 0)?;
    let (physical, size) = (call.arg(1) as u64, call.arg(2) as u64);
    if !resource.covers_mmio(physical, size) {
        return Err(native_abi::ERR_ACCESS_DENIED);
    }
    let vmo = Vmo::create_physical(physical, size, CachePolicy::Device).map_err(vm_status)?;
    call.publish(Object::Vmo(vmo), call.arg(3)).map(|_| ())
}

fn permissions(options: u32) -> PagePermissions {
    let mut permissions = PagePermissions::empty();
    if options & vm::READ != 0 {
        permissions |= PagePermissions::READ;
    }
    if options & vm::WRITE != 0 {
        permissions |= PagePermissions::WRITE;
    }
    if options & vm::EXECUTE != 0 {
        permissions |= PagePermissions::EXECUTE;
    }
    permissions
}

fn ceiling(handle_rights: u32) -> PagePermissions {
    let mut ceiling = PagePermissions::empty();
    if handle_rights & rights::READ != 0 {
        ceiling |= PagePermissions::READ;
    }
    if handle_rights & rights::WRITE != 0 {
        ceiling |= PagePermissions::WRITE;
    }
    if handle_rights & rights::EXECUTE != 0 {
        ceiling |= PagePermissions::EXECUTE;
    }
    ceiling
}

fn placement(options: u32, offset: usize) -> Result<crate::vm::Placement, Status> {
    match (
        options & vm::SPECIFIC != 0,
        options & vm::SPECIFIC_OVERWRITE != 0,
    ) {
        (false, false) => Ok(crate::vm::Placement::Any),
        (true, false) => Ok(crate::vm::Placement::Specific(offset)),
        (false, true) => Ok(crate::vm::Placement::Overwrite(offset)),
        (true, true) => Err(native_abi::ERR_INVALID_ARGS),
    }
}

const MAP_OPTIONS: u32 =
    vm::READ | vm::WRITE | vm::EXECUTE | vm::SPECIFIC | vm::SPECIFIC_OVERWRITE | vm::ALLOW_FAULTS;

pub(super) fn vmar_map(call: &Call<'_>) -> Result<(), Status> {
    let options = call.arg_u32(1);
    if options & !MAP_OPTIONS != 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let vmar = call.vmar(call.arg_handle(0), rights::MAP)?;
    let entry = call.entry(call.arg_handle(3), rights::MAP)?;
    let Object::Vmo(vmo) = &entry.object else {
        return Err(native_abi::ERR_WRONG_TYPE);
    };
    let requested = permissions(options);
    let ceiling = ceiling(entry.rights);
    if !ceiling.contains(requested) {
        return Err(native_abi::ERR_ACCESS_DENIED);
    }
    let address = vmar
        .map(
            vmo,
            call.arg(4) as u64,
            call.arg(5),
            MapOptions {
                placement: placement(options, call.arg(2))?,
                permissions: requested,
                ceiling,
                allow_faults: options & vm::ALLOW_FAULTS != 0,
            },
        )
        .map_err(vm_status)?;
    if let Err(status) = call.write(call.arg(6), &(address as u64)) {
        let _ = vmar.unmap(address, call.arg(5));
        return Err(status);
    }
    Ok(())
}

pub(super) fn vmar_unmap(call: &Call<'_>) -> Result<(), Status> {
    let vmar = call.vmar(call.arg_handle(0), rights::MAP)?;
    vmar.unmap(call.arg(1), call.arg(2)).map_err(vm_status)
}

pub(super) fn vmar_protect(call: &Call<'_>) -> Result<(), Status> {
    let vmar = call.vmar(call.arg_handle(0), rights::MAP)?;
    vmar.protect(call.arg(2), call.arg(3), permissions(call.arg_u32(1)))
        .map_err(vm_status)
}

pub(super) fn vmar_allocate(call: &Call<'_>) -> Result<(), Status> {
    let options = call.arg_u32(1);
    let parent = call.vmar(call.arg_handle(0), rights::MANAGE)?;
    let child = parent
        .allocate(
            placement(options, call.arg(2))?,
            call.arg(3),
            permissions(options),
        )
        .map_err(vm_status)?;
    let base = child.info().base as u64;
    let handle = call.publish(Object::Vmar(child.clone()), call.arg(4))?;
    if let Err(status) = call.write(call.arg(5), &base) {
        drop(call.process().handles().remove(handle));
        let _ = child.destroy();
        return Err(status);
    }
    Ok(())
}

pub(super) fn vmar_destroy(call: &Call<'_>) -> Result<(), Status> {
    call.vmar(call.arg_handle(0), rights::MANAGE)?
        .destroy()
        .map_err(vm_status)
}

/// 调试器的跨进程访问：按目标映射背后的 VMO 读写（见 [`crate::vm::AddressSpace::access_debug`]）。
fn cross_process(call: &Call<'_>, write: bool) -> Result<(), Status> {
    let process = call.process_handle(
        call.arg_handle(0),
        if write { rights::WRITE } else { rights::READ },
    )?;
    let target = process.space().clone();
    let (address, buffer, length) = (call.arg(1), call.arg(2), call.arg(3));
    let mut done = 0;
    while done < length {
        let chunk = (length - done).min(CHUNK);
        let mut bytes = if write {
            call.read_bytes(buffer + done, chunk)?
        } else {
            let mut bytes = alloc::vec::Vec::new();
            bytes
                .try_reserve_exact(chunk)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            bytes.resize(chunk, 0);
            bytes
        };
        let Some(start) = address.checked_add(done) else {
            break;
        };
        let accessed = target.access_debug(start, &mut bytes, write);
        if !write {
            call.write_bytes(buffer + done, &bytes[..accessed])?;
        }
        done += accessed;
        if accessed < chunk {
            break;
        }
    }
    call.write_optional(call.arg(4), &done)?;
    if done == 0 && length != 0 {
        return Err(native_abi::ERR_NOT_FOUND);
    }
    Ok(())
}

pub(super) fn process_read_memory(call: &Call<'_>) -> Result<(), Status> {
    cross_process(call, false)
}

pub(super) fn process_write_memory(call: &Call<'_>) -> Result<(), Status> {
    cross_process(call, true)
}

pub(super) fn pager_create(call: &Call<'_>) -> Result<(), Status> {
    let pager = Arc::try_new(Pager::new()).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    call.publish(Object::Pager(pager), call.arg(0)).map(|_| ())
}

pub(super) fn pager_create_vmo(call: &Call<'_>) -> Result<(), Status> {
    let pager = call.pager(call.arg_handle(0), rights::MANAGE)?;
    let options = call.arg_u32(1);
    if options & !vmo::RESIZABLE != 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let port = call.port(call.arg_handle(2), rights::WRITE)?;
    let vmo = pager.create_vmo(
        &port,
        call.arg(3) as u64,
        call.arg(4) as u64,
        options & vmo::RESIZABLE != 0,
    )?;
    call.publish(Object::Vmo(vmo), call.arg(5)).map(|_| ())
}

pub(super) fn pager_supply_pages(call: &Call<'_>) -> Result<(), Status> {
    let _pager = call.pager(call.arg_handle(0), rights::MANAGE)?;
    let vmo = call.vmo(call.arg_handle(1), rights::WRITE)?;
    let aux = call.vmo(call.arg_handle(4), rights::READ | rights::WRITE)?;
    vmo.supply_pages(
        call.arg(2) as u64,
        call.arg(3) as u64,
        &aux,
        call.arg(5) as u64,
    )
    .map_err(vm_status)
}

pub(super) fn pager_op(call: &Call<'_>) -> Result<(), Status> {
    let _pager = call.pager(call.arg_handle(0), rights::MANAGE)?;
    let vmo = call.vmo(call.arg_handle(2), rights::WRITE)?;
    let (offset, length) = (call.arg(3) as u64, call.arg(4) as u64);
    match call.arg_u32(1) {
        native_abi::pager::OP_FAIL => vmo.fail_range(offset, length).map_err(vm_status),
        native_abi::pager::OP_WRITEBACK_BEGIN => {
            vmo.begin_writeback(offset, length).map_err(vm_status)
        }
        native_abi::pager::OP_WRITEBACK_END => vmo.end_writeback(offset, length).map_err(vm_status),
        native_abi::pager::OP_DETACH => vmo.detach_pager().map_err(vm_status),
        native_abi::pager::OP_QUERY_DIRTY => {
            let range = vmo.first_dirty_range(offset, length).map_err(vm_status)?;
            let value: [u64; 2] = range.map_or([0, 0], |(start, size)| [start, size]);
            call.write_array(call.arg(5), &value)
        }
        _ => Err(native_abi::ERR_INVALID_ARGS),
    }
}
