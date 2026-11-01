//! 句柄与通用对象调用。

use native_abi::{HandleBasic, ProcessBasic, Status, TaskRuntime, VmarRegion, VmoInfo, info, rights};

use super::{Call, Plain, bytes_of};
use crate::object::{HandleEntry, Object, WaitOutcome};

pub(super) fn close(call: &Call<'_>) -> Result<(), Status> {
    let handle = call.arg_handle(0);
    if handle == native_abi::HANDLE_INVALID {
        return Ok(());
    }
    let handles = call.process().handles();
    let entry = handles.remove(handle)?;
    entry.object.kernel_object().cancel(handles.cancel_key(handle));
    drop(entry);
    Ok(())
}

fn narrowed(entry: &HandleEntry, requested: u32) -> Result<u32, Status> {
    if requested == rights::SAME {
        return Ok(entry.rights);
    }
    if requested & !entry.rights != 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok(requested)
}

pub(super) fn duplicate(call: &Call<'_>) -> Result<(), Status> {
    let entry = call.entry(call.arg_handle(0), rights::DUPLICATE)?;
    let rights = narrowed(&entry, call.arg_u32(1))?;
    let handles = call.process().handles();
    let handle = handles.insert(HandleEntry {
        object: entry.object,
        rights,
    })?;
    if let Err(status) = call.write(call.arg(2), &handle) {
        drop(handles.remove(handle));
        return Err(status);
    }
    Ok(())
}

pub(super) fn replace(call: &Call<'_>) -> Result<(), Status> {
    let handles = call.process().handles();
    let old = call.arg_handle(0);
    let entry = handles.get(old)?;
    let rights = narrowed(&entry, call.arg_u32(1))?;
    let handle = handles.insert(HandleEntry {
        object: entry.object.clone(),
        rights,
    })?;
    if let Err(status) = call.write(call.arg(2), &handle) {
        drop(handles.remove(handle));
        return Err(status);
    }
    let removed = handles.remove(old)?;
    removed.object.kernel_object().cancel(handles.cancel_key(old));
    Ok(())
}

pub(super) fn wait_one(call: &Call<'_>) -> Result<(), Status> {
    let handle = call.arg_handle(0);
    let entry = call.entry(handle, rights::WAIT)?;
    let signals = entry
        .object
        .kernel_object()
        .signals()
        .ok_or(native_abi::ERR_NOT_SUPPORTED)?;
    let key = call.process().handles().cancel_key(handle);
    let (status, observed) = match signals.wait(call.arg_u32(1), key, super::deadline(call.arg(2)))? {
        WaitOutcome::Satisfied(observed) => (native_abi::OK, observed),
        WaitOutcome::TimedOut(observed) => (native_abi::ERR_TIMED_OUT, observed),
        WaitOutcome::Canceled | WaitOutcome::Interrupted => (native_abi::ERR_CANCELED, 0),
    };
    call.write_optional(call.arg(3), &observed)?;
    if status == native_abi::OK { Ok(()) } else { Err(status) }
}

pub(super) fn wait_async(call: &Call<'_>) -> Result<(), Status> {
    let handle = call.arg_handle(0);
    let entry = call.entry(handle, rights::WAIT)?;
    let port = call.port(call.arg_handle(1), rights::WRITE)?;
    let signals = entry
        .object
        .kernel_object()
        .signals()
        .ok_or(native_abi::ERR_NOT_SUPPORTED)?;
    let key = call.process().handles().cancel_key(handle);
    signals.wait_async(call.arg_u32(3), key, &port, call.arg(2) as u64)
}

pub(super) fn signal(call: &Call<'_>) -> Result<(), Status> {
    let entry = call.entry(call.arg_handle(0), rights::SIGNAL)?;
    let object = entry.object.kernel_object();
    let mask = object.user_signal_mask();
    let (clear, set) = (call.arg_u32(1), call.arg_u32(2));
    if (clear | set) & !mask != 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    object
        .signals()
        .ok_or(native_abi::ERR_NOT_SUPPORTED)?
        .update(clear, set);
    Ok(())
}

fn write_info<T: Plain>(call: &Call<'_>, value: &T) -> Result<(), Status> {
    let bytes = bytes_of(value);
    call.write_optional(call.arg(4), &bytes.len())?;
    if call.arg(3) < bytes.len() {
        return Err(native_abi::ERR_BUFFER_TOO_SMALL);
    }
    call.write_bytes(call.arg(2), bytes)
}

pub(super) fn get_info(call: &Call<'_>) -> Result<(), Status> {
    let entry = call.entry(call.arg_handle(0), rights::INSPECT)?;
    match call.arg_u32(1) {
        info::HANDLE_BASIC => {
            let object = entry.object.kernel_object();
            write_info(
                call,
                &HandleBasic {
                    koid: object.koid(),
                    object_type: object.object_type() as u32,
                    rights: entry.rights,
                },
            )
        }
        info::TASK_RUNTIME => {
            let runtime = match &entry.object {
                Object::Thread(thread) => thread.runtime(),
                Object::Process(process) => process.runtime(),
                _ => return Err(native_abi::ERR_WRONG_TYPE),
            };
            write_info(
                call,
                &TaskRuntime {
                    cpu_time_ns: runtime.cpu_us.saturating_mul(1000),
                    restricted_time_ns: runtime.restricted_us.saturating_mul(1000),
                    voluntary_switches: runtime.voluntary_switches,
                    involuntary_switches: runtime.involuntary_switches,
                },
            )
        }
        info::VMO => match &entry.object {
            Object::Vmo(vmo) => write_info(
                call,
                &VmoInfo {
                    size: vmo.size(),
                    committed_bytes: vmo.committed_bytes().map_err(crate::object::vm_status)?,
                },
            ),
            _ => Err(native_abi::ERR_WRONG_TYPE),
        },
        info::VMAR => match &entry.object {
            Object::Vmar(vmar) => {
                let region = vmar.info();
                write_info(
                    call,
                    &VmarRegion {
                        base: region.base as u64,
                        size: region.size as u64,
                    },
                )
            }
            _ => Err(native_abi::ERR_WRONG_TYPE),
        },
        info::PROCESS => match &entry.object {
            Object::Process(process) => {
                let (exited, code) = process.exit_status();
                write_info(
                    call,
                    &ProcessBasic {
                        exited: u32::from(exited),
                        return_code: code as i32,
                    },
                )
            }
            _ => Err(native_abi::ERR_WRONG_TYPE),
        },
        info::NAME => {
            let (name, length) = match &entry.object {
                Object::Process(process) => process.name(),
                Object::Thread(thread) => thread.name(),
                _ => return Err(native_abi::ERR_WRONG_TYPE),
            };
            call.write_optional(call.arg(4), &length)?;
            if call.arg(3) < length {
                return Err(native_abi::ERR_BUFFER_TOO_SMALL);
            }
            call.write_bytes(call.arg(2), &name[..length])
        }
        _ => Err(native_abi::ERR_NOT_SUPPORTED),
    }
}

pub(super) fn set_name(call: &Call<'_>) -> Result<(), Status> {
    let length = call.arg(2);
    if length > 32 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let name = call.read_bytes(call.arg(1), length)?;
    match call.entry(call.arg_handle(0), rights::MANAGE)?.object {
        Object::Process(process) => process.set_name(&name),
        Object::Thread(thread) => thread.set_name(&name),
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    }
    Ok(())
}
