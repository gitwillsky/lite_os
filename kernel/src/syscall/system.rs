//! 日志、串口、时钟、随机数、系统信息与电源调用。

use native_abi::{DebuglogRecord, Status, SystemBasic};

use super::Call;
use crate::execution::sync::WaitResult;
use native_abi::resource::system as authority;

fn require_resource(call: &Call<'_>, operation: u64) -> Result<(), Status> {
    if call
        .resource(call.arg_handle(0), 0)?
        .covers_system(operation)
    {
        Ok(())
    } else {
        Err(native_abi::ERR_ACCESS_DENIED)
    }
}

/// 取得系统事件；句柄去掉 `SIGNAL` 权限，持有者只能等待。
pub(super) fn get_event(call: &Call<'_>) -> Result<(), Status> {
    use crate::object::Object;
    require_resource(call, authority::MEMORY)?;
    if call.arg_u32(1) != native_abi::system::EVENT_MEMORY_PRESSURE {
        return Err(native_abi::ERR_NOT_SUPPORTED);
    }
    let event = crate::object::memory_pressure_event().map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let mut entry = Object::Event(event).into_entry();
    entry.rights &= !native_abi::rights::SIGNAL;
    let handle = call.process().handles().insert(entry)?;
    if let Err(status) = call.write(call.arg(2), &handle) {
        drop(call.process().handles().remove(handle));
        return Err(status);
    }
    Ok(())
}

fn wait_status(result: WaitResult) -> Status {
    match result {
        WaitResult::TimedOut => native_abi::ERR_TIMED_OUT,
        WaitResult::Interrupted => native_abi::ERR_CANCELED,
        WaitResult::OutOfMemory => native_abi::ERR_NO_MEMORY,
        WaitResult::Woken => native_abi::OK,
    }
}

pub(super) fn debuglog_write(call: &Call<'_>) -> Result<(), Status> {
    let length = call.arg(2).min(crate::log::MESSAGE_CAPACITY);
    let text = call.read_bytes(call.arg(1), length)?;
    crate::log::write_external(
        crate::log::LogLevel::from_raw(call.arg(0) as u8),
        call.arg(3) as u64,
        &text,
    );
    Ok(())
}

pub(super) fn debuglog_read(call: &Call<'_>) -> Result<(), Status> {
    require_resource(call, authority::DEBUGLOG)?;
    let deadline = match call.arg(3) as i64 {
        0 => None,
        _ => Some(super::deadline(call.arg(3)).unwrap_or(u64::MAX)),
    };
    let record = crate::log::read_record(call.arg(1) as u64, deadline).map_err(wait_status)?;
    match record {
        crate::log::ReadRecord::Record(record) => {
            let mut output = DebuglogRecord {
                sequence: record.sequence,
                timestamp_ns: record.timestamp_us.saturating_mul(1000),
                severity: record.level as u32,
                annotation: record.annotation,
                length: record.length as u32,
                message: [0; native_abi::DEBUGLOG_MESSAGE_CAPACITY],
            };
            output.message[..record.length].copy_from_slice(&record.message[..record.length]);
            call.write_bytes(call.arg(2), super::bytes_of(&output))
        }
        crate::log::ReadRecord::Overrun(oldest) => {
            call.write(call.arg(2), &oldest)?;
            Err(native_abi::ERR_OUT_OF_RANGE)
        }
        crate::log::ReadRecord::Empty => Err(native_abi::ERR_SHOULD_WAIT),
    }
}

pub(super) fn serial_read(call: &Call<'_>) -> Result<(), Status> {
    require_resource(call, authority::SERIAL)?;
    let capacity = call.arg(2).min(4096);
    let mut buffer = alloc::vec::Vec::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    buffer.resize(capacity, 0);
    let deadline = match call.arg(3) as i64 {
        0 => None,
        _ => Some(super::deadline(call.arg(3)).unwrap_or(u64::MAX)),
    };
    let count = crate::serial::read(&mut buffer, deadline).map_err(wait_status)?;
    call.write_bytes(call.arg(1), &buffer[..count])?;
    call.write_optional(call.arg(4), &count)
}

pub(super) fn serial_write(call: &Call<'_>) -> Result<(), Status> {
    require_resource(call, authority::SERIAL)?;
    let (mut address, mut remaining) = (call.arg(1), call.arg(2));
    while remaining != 0 {
        let chunk = remaining.min(4096);
        let bytes = call.read_bytes(address, chunk)?;
        crate::platform::console::write_raw(&bytes);
        address += chunk;
        remaining -= chunk;
    }
    Ok(())
}

pub(super) fn barrier() -> Result<(), Status> {
    crate::execution::barrier::synchronize();
    Ok(())
}

pub(super) fn clock_get_monotonic(call: &Call<'_>) -> Result<(), Status> {
    call.write(
        call.arg(0),
        &(crate::execution::timer::get_time_ns() as i64),
    )
}

pub(super) fn clock_get_utc(call: &Call<'_>) -> Result<(), Status> {
    call.write(
        call.arg(0),
        &(crate::execution::timer::get_realtime_ns() as i64),
    )
}

pub(super) fn clock_set_utc(call: &Call<'_>) -> Result<(), Status> {
    require_resource(call, authority::UTC)?;
    let utc = u64::try_from(call.arg(1) as i64).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    crate::execution::timer::set_realtime_ns(utc).map_err(|()| native_abi::ERR_INVALID_ARGS)
}

pub(super) fn nanosleep(call: &Call<'_>) -> Result<(), Status> {
    let deadline = super::deadline(call.arg(0)).unwrap_or(u64::MAX);
    if deadline <= crate::execution::timer::get_time_ns() {
        // 截止时间已过：让出 CPU（与 Zircon `zx_nanosleep(0)` 相同，即 yield）。
        crate::thread::yield_current();
        return Ok(());
    }
    match crate::thread::sleep_until(deadline) {
        WaitResult::TimedOut | WaitResult::Woken => Ok(()),
        other => Err(wait_status(other)),
    }
}

pub(super) fn cprng_draw(call: &Call<'_>) -> Result<(), Status> {
    let length = call.arg(1);
    let mut buffer = [0u8; 256];
    let mut done = 0;
    while done < length {
        let chunk = (length - done).min(buffer.len());
        crate::random::fill(&mut buffer[..chunk]);
        call.write_bytes(call.arg(0) + done, &buffer[..chunk])?;
        done += chunk;
    }
    Ok(())
}

pub(super) fn cprng_add_entropy(call: &Call<'_>) -> Result<(), Status> {
    let length = call.arg(1);
    if length > crate::random::MAX_ENTROPY_INPUT {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let bytes = call.read_bytes(call.arg(0), length)?;
    crate::random::add_entropy(&bytes);
    Ok(())
}

pub(super) fn get_info(call: &Call<'_>) -> Result<(), Status> {
    let (buffer, capacity, actual) = (call.arg(1), call.arg(2), call.arg(3));
    match call.arg_u32(0) {
        native_abi::system::BASIC => {
            let frames = crate::memory::frame_statistics();
            let info = SystemBasic {
                cpu_count: crate::execution::cpu::count() as u32,
                page_size: native_abi::PAGE_SIZE as u32,
                total_pages: frames.capacity_pages as u64,
                free_pages: frames.free_pages as u64,
                boot_utc_ns: (crate::execution::timer::boot_epoch_seconds() as i64)
                    .saturating_mul(1_000_000_000),
                clock_resolution_ns: crate::execution::timer::monotonic_resolution_ns(),
                scheduler_quantum_ns: crate::execution::timer::scheduler_quantum_ns(),
            };
            let bytes = super::bytes_of(&info);
            call.write_optional(actual, &bytes.len())?;
            if capacity < bytes.len() {
                return Err(native_abi::ERR_BUFFER_TOO_SMALL);
            }
            call.write_bytes(buffer, bytes)
        }
        native_abi::system::FREE_BLOCKS => {
            let blocks = crate::memory::frame_allocator::statistics().free_blocks;
            let values: [u64; crate::memory::frame_allocator::ORDER_COUNT] =
                blocks.map(|count| count as u64);
            let size = values.len() * 8;
            call.write_optional(actual, &size)?;
            if capacity < size {
                return Err(native_abi::ERR_BUFFER_TOO_SMALL);
            }
            call.write_array(buffer, &values)
        }
        native_abi::system::CPU_BUSY => {
            let busy =
                crate::thread::cpu_busy_snapshot().map_err(|()| native_abi::ERR_NO_MEMORY)?;
            let mut values = alloc::vec::Vec::new();
            values
                .try_reserve_exact(busy.len())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            values.extend(busy.iter().map(|us| us.saturating_mul(1000)));
            let size = values.len() * 8;
            call.write_optional(actual, &size)?;
            if capacity < size {
                return Err(native_abi::ERR_BUFFER_TOO_SMALL);
            }
            call.write_array(buffer, &values)
        }
        native_abi::system::MEMORY_STATS => {
            let vmo = crate::vm::physical_page_statistics();
            let stats = native_abi::SystemMemoryStats {
                heap_pages: crate::memory::heap_pages() as u64,
                kernel_stack_pages: crate::vm::kernel_stack::mapped_pages() as u64,
                page_table_pages: crate::vm::page_table::table_pages() as u64,
                evicted_pages: crate::vm::evicted_pages() as u64,
                vmo_anonymous_pages: vmo.anonymous_pages as u64,
                vmo_pager_pages: vmo.pager_pages as u64,
                vmo_contiguous_pages: vmo.contiguous_pages as u64,
                bootfs_pages: vmo.bootfs_pages as u64,
                pager_dirty_pages: vmo.pager_dirty_pages as u64,
                pager_writeback_pages: vmo.pager_writeback_pages as u64,
                pager_reclaimable_pages: vmo.pager_reclaimable_pages as u64,
            };
            let bytes = super::bytes_of(&stats);
            call.write_optional(actual, &bytes.len())?;
            if capacity < bytes.len() {
                return Err(native_abi::ERR_BUFFER_TOO_SMALL);
            }
            call.write_bytes(buffer, bytes)
        }
        native_abi::system::CRASH_LOG => {
            let log = crate::crash::previous_dump().unwrap_or(&[]);
            call.write_optional(actual, &log.len())?;
            if capacity < log.len() {
                return Err(native_abi::ERR_BUFFER_TOO_SMALL);
            }
            call.write_bytes(buffer, log)
        }
        _ => Err(native_abi::ERR_NOT_SUPPORTED),
    }
}

pub(super) fn power(call: &Call<'_>) -> Result<(), Status> {
    require_resource(call, authority::POWER)?;
    let kind = match call.arg_u32(1) {
        native_abi::system::POWER_OFF => crate::platform::ResetKind::Shutdown,
        native_abi::system::POWER_REBOOT => crate::platform::ResetKind::ColdReboot,
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    };
    crate::platform::reset_system(kind, crate::platform::ResetReason::Requested)
        .map_err(|_| native_abi::ERR_INTERNAL)
}
