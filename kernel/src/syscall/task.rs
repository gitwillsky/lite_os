//! Job、进程、线程与异常调用。

use alloc::sync::Arc;

use native_abi::{Status, rights};

use super::{Call, Outcome};
use crate::object::{Channel, ExceptionResolution, Object, Process, ProcessKind, Thread};
use crate::thread::UserStart;

const NAME_LIMIT: usize = 32;

fn name(call: &Call<'_>, address: usize, length: usize) -> Result<alloc::vec::Vec<u8>, Status> {
    if length > NAME_LIMIT {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    call.read_bytes(address, length)
}

pub(super) fn job_create(call: &Call<'_>) -> Result<(), Status> {
    let parent = call.job(call.arg_handle(0), rights::MANAGE)?;
    let child = parent.create_child()?;
    call.publish(Object::Job(child), call.arg(1)).map(|_| ())
}

fn publish_process(call: &Call<'_>, process: Arc<Process>, process_out: usize, vmar_out: usize) -> Result<(), Status> {
    let vmar = process.root_vmar().clone();
    let handle = call.publish(Object::Process(process.clone()), process_out)?;
    if let Err(status) = call.publish(Object::Vmar(vmar), vmar_out) {
        drop(call.process().handles().remove(handle));
        process.terminate(None);
        return Err(status);
    }
    Ok(())
}

pub(super) fn process_create(call: &Call<'_>) -> Result<(), Status> {
    let job = call.job(call.arg_handle(0), rights::MANAGE)?;
    let name = name(call, call.arg(1), call.arg(2))?;
    let kind = match call.arg_u32(3) {
        0 => ProcessKind::Plain,
        native_abi::process::SHARED => ProcessKind::Shared,
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    };
    let process = Process::create(&job, &name, kind)?;
    publish_process(call, process, call.arg(4), call.arg(5))
}

pub(super) fn process_create_shared(call: &Call<'_>) -> Result<(), Status> {
    let shared = call.process_handle(call.arg_handle(0), rights::MANAGE)?;
    let name = name(call, call.arg(1), call.arg(2))?;
    let process = Process::create(shared.job(), &name, ProcessKind::Member(&shared))?;
    publish_process(call, process, call.arg(3), call.arg(4))
}

fn start_thread(thread: &Arc<Thread>, entry: usize, stack: usize, arguments: [usize; 2]) -> Result<(), Status> {
    thread.start(
        thread.clone(),
        UserStart {
            entry,
            stack,
            arguments,
        },
    )
}

pub(super) fn process_start(call: &Call<'_>) -> Result<(), Status> {
    let process = call.process_handle(call.arg_handle(0), rights::MANAGE)?;
    let thread = call.thread_handle(call.arg_handle(1), rights::MANAGE)?;
    if !Arc::ptr_eq(thread.process(), &process) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    // 参数句柄无论成功与否都被消费（与 Zircon 相同），避免失败路径上调用者无法判断其归属。
    let transferred = match call.arg_handle(4) {
        native_abi::HANDLE_INVALID => None,
        handle => {
            let table = call.process().handles();
            let entry = table.remove(handle)?;
            entry.object.kernel_object().cancel(table.cancel_key(handle));
            if entry.rights & rights::TRANSFER == 0 {
                return Err(native_abi::ERR_ACCESS_DENIED);
            }
            Some(entry)
        }
    };
    process.mark_started()?;
    let argument = match transferred {
        Some(entry) => process.handles().insert(entry)? as usize,
        None => 0,
    };
    if let Err(status) = start_thread(&thread, call.arg(2), call.arg(3), [argument, call.arg(5)]) {
        if argument != 0 {
            drop(process.handles().remove(argument as native_abi::Handle));
        }
        process.terminate(None);
        return Err(status);
    }
    Ok(())
}

pub(super) fn process_exit(call: &Call<'_>) -> Outcome {
    call.process().terminate(Some(call.arg(0) as i64));
    Outcome::Exit
}

pub(super) fn thread_create(call: &Call<'_>) -> Result<(), Status> {
    let process = call.process_handle(call.arg_handle(0), rights::MANAGE)?;
    let name = name(call, call.arg(1), call.arg(2))?;
    let thread = Thread::create(&process, &name)?;
    call.publish(Object::Thread(thread), call.arg(3)).map(|_| ())
}

pub(super) fn thread_start(call: &Call<'_>) -> Result<(), Status> {
    let thread = call.thread_handle(call.arg_handle(0), rights::MANAGE)?;
    start_thread(&thread, call.arg(1), call.arg(2), [call.arg(3), call.arg(4)])
}

pub(super) fn task_kill(call: &Call<'_>) -> Outcome {
    let entry = match call.entry(call.arg_handle(0), rights::MANAGE) {
        Ok(entry) => entry,
        Err(status) => return Outcome::Return(status),
    };
    let own_process = call.process().clone();
    match entry.object {
        Object::Job(job) => job.kill(),
        Object::Process(process) => {
            process.terminate(None);
            if Arc::ptr_eq(&process, &own_process) {
                return Outcome::Exit;
            }
        }
        Object::Thread(thread) => {
            let own = Arc::ptr_eq(&thread, call.owner);
            thread.kill();
            if own {
                return Outcome::Exit;
            }
        }
        _ => return Outcome::Return(native_abi::ERR_WRONG_TYPE),
    }
    // 自身所在 Job 被 kill 时，返回用户态前观察到终止。
    Outcome::Return(native_abi::OK)
}

pub(super) fn thread_set_priority(call: &Call<'_>) -> Result<(), Status> {
    let thread = call.thread_handle(call.arg_handle(0), rights::MANAGE)?;
    let priority = u8::try_from(call.arg(1)).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    thread.set_priority(priority)
}

pub(super) fn thread_set_affinity(call: &Call<'_>) -> Result<(), Status> {
    let thread = call.thread_handle(call.arg_handle(0), rights::MANAGE)?;
    let cpus = crate::execution::cpu::CpuSet::from_native_word(call.arg(1));
    thread.set_affinity(cpus)?;
    // 调用线程限制了自己且当前 CPU 已不允许：返回前让出 CPU，重新调度时迁移到允许的 CPU。
    if thread.execution().is_some_and(|execution| Arc::ptr_eq(&execution, call.thread))
        && !cpus.contains(crate::execution::cpu::current_id())
    {
        crate::thread::yield_current();
    }
    Ok(())
}

pub(super) fn cpu_current(call: &Call<'_>) -> Result<(), Status> {
    let cpu = crate::execution::cpu::current_id().index() as u32;
    call.write(call.arg(0), &cpu)
}

pub(super) fn create_exception_channel(call: &Call<'_>) -> Result<(), Status> {
    let entry = call.entry(call.arg_handle(0), rights::MANAGE)?;
    let (kernel, user) = Channel::create_pair()?;
    let kernel = Arc::try_new(kernel).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let user = Arc::try_new(user).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    match &entry.object {
        Object::Process(process) => process.bind_exception_channel(kernel)?,
        Object::Job(job) => job.bind_exception_channel(kernel)?,
        _ => return Err(native_abi::ERR_WRONG_TYPE),
    }
    let handle = call.process().handles().insert(crate::object::HandleEntry {
        object: Object::Channel(user),
        rights: rights::READ | rights::WAIT | rights::TRANSFER | rights::INSPECT,
    })?;
    if let Err(status) = call.write(call.arg(1), &handle) {
        drop(call.process().handles().remove(handle));
        return Err(status);
    }
    Ok(())
}

pub(super) fn exception_resolve(call: &Call<'_>) -> Result<(), Status> {
    let exception = call.exception(call.arg_handle(0), rights::WRITE)?;
    let resolution = if call.arg(1) == 1 {
        let state = match call.arg(2) {
            0 => None,
            address => Some(
                alloc::boxed::Box::try_new(call.read(address)?).map_err(|_| native_abi::ERR_NO_MEMORY)?,
            ),
        };
        ExceptionResolution::Resume(state)
    } else {
        ExceptionResolution::TryNext
    };
    exception.resolve(resolution);
    Ok(())
}
