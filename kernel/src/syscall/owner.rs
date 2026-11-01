//! 用户线程的 trap 语义（`object::Thread` 实现 `ThreadOwner`）。
//!
//! # syscall
//!
//! normal mode 的 syscall 按 native 编号分派；restricted mode 的 syscall 不解释，直接退回 normal mode。
//!
//! # 同步异常
//!
//! 1. 缺页先交给地址空间解决；成功则重试原指令。
//! 2. restricted mode 中仍未解决的异常退回 normal mode（原因 EXCEPTION），由 normal mode 代码处理。
//! 3. normal mode 中的异常依次交给进程、所属 Job 及祖先 Job 的异常 channel；全部未处理则终止进程。
//!
//! # 返回用户态前
//!
//! 线程被 kill 时退出；restricted mode 中被 kick 时退回 normal mode。

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_abi::ExceptionReport;

use super::{Call, Outcome, bytes_of, restricted};
use crate::object::{Channel, Exception, ExceptionResolution, HandleEntry, Message, Mode, Object};
use crate::thread::{Disposition, Thread, ThreadOwner, UserException};

impl ThreadOwner for crate::object::Thread {
    fn syscall(self: Arc<Self>, thread: &Arc<Thread>) -> Disposition {
        let request = thread.with_user_context(|context| context.take_syscall_request());
        if let Mode::Restricted(entry) = self.mode() {
            return continue_unless(restricted::exit(
                &self,
                thread,
                entry,
                native_abi::restricted::REASON_SYSCALL,
                None,
            ));
        }
        let call = Call {
            thread,
            owner: &self,
            arguments: request.arguments,
        };
        match super::dispatch(&call, request.number) {
            Outcome::Return(status) => {
                thread.with_user_context(|context| context.set_syscall_result(status as isize as usize));
                Disposition::Continue
            }
            Outcome::Preserve => Disposition::Continue,
            Outcome::Exit => Disposition::Exit,
        }
    }

    fn exception(self: Arc<Self>, thread: &Arc<Thread>, exception: UserException) -> Disposition {
        if let UserException::PageFault { address, access, .. } = exception
            && thread
                .address_space()
                .is_some_and(|space| space.fault(address, access).is_ok())
        {
            return Disposition::Continue;
        }
        if thread.is_killed() {
            return Disposition::Exit;
        }
        let report = report(thread, &self, exception);
        if let Mode::Restricted(entry) = self.mode() {
            return continue_unless(restricted::exit(
                &self,
                thread,
                entry,
                native_abi::restricted::REASON_EXCEPTION,
                Some(report),
            ));
        }
        match deliver(&self, report) {
            Delivery::Resume(state) => {
                if let Some(state) = state
                    && thread
                        .with_user_context(|context| context.load_registers(&state))
                        .is_err()
                {
                    self.process().terminate(None);
                    return Disposition::Exit;
                }
                Disposition::Continue
            }
            Delivery::Killed => Disposition::Exit,
            Delivery::Unhandled => {
                let (name, length) = self.process().name();
                warn!(
                    "unhandled exception kind={} at {:#x} (syndrome {:#x}) in process {}; terminating",
                    report.kind,
                    report.address,
                    report.syndrome,
                    core::str::from_utf8(&name[..length]).unwrap_or("<non-utf8>")
                );
                self.process().terminate(None);
                Disposition::Exit
            }
        }
    }

    fn before_user_return(self: Arc<Self>, thread: &Arc<Thread>) -> Disposition {
        if thread.is_killed() {
            return Disposition::Exit;
        }
        if let Mode::Restricted(entry) = self.mode()
            && thread.take_kick()
        {
            return continue_unless(restricted::exit(
                &self,
                thread,
                entry,
                native_abi::restricted::REASON_KICK,
                None,
            ));
        }
        Disposition::Continue
    }

    fn exited(self: Arc<Self>, _thread: &Arc<Thread>) {
        crate::object::Thread::exited(&self);
    }
}

fn continue_unless(written: bool) -> Disposition {
    if written {
        Disposition::Continue
    } else {
        Disposition::Exit
    }
}

fn report(thread: &Arc<Thread>, owner: &crate::object::Thread, exception: UserException) -> ExceptionReport {
    use native_abi::exception as kind;
    let (kind, access, address, syndrome) = match exception {
        UserException::PageFault {
            address,
            access,
            syndrome,
        } => (
            kind::PAGE_FAULT,
            match access {
                crate::vm::FaultAccess::Read => 0,
                crate::vm::FaultAccess::Write => 1,
                crate::vm::FaultAccess::Execute => 2,
            },
            address,
            syndrome,
        ),
        UserException::AccessFault { address, syndrome } => (kind::ACCESS_FAULT, 0, address, syndrome),
        UserException::Alignment { address, syndrome } => (kind::ALIGNMENT, 0, address, syndrome),
        UserException::IllegalInstruction { pc } => (kind::ILLEGAL_INSTRUCTION, 0, pc, 0),
        UserException::Breakpoint { pc } => (kind::BREAKPOINT, 0, pc, 0),
        UserException::Unsupported { pc } => (kind::UNSUPPORTED, 0, pc, 0),
    };
    ExceptionReport {
        kind,
        access,
        address: address as u64,
        syndrome,
        thread_koid: crate::object::KernelObject::koid(owner),
        state: thread.with_user_context(|context| context.save_registers()),
    }
}

enum Delivery {
    Resume(Option<alloc::boxed::Box<native_abi::register::RegisterState>>),
    Killed,
    Unhandled,
}

/// 依次尝试进程、Job 与祖先 Job 的异常 channel。
fn deliver(owner: &crate::object::Thread, report: ExceptionReport) -> Delivery {
    let process = owner.process();
    if let Some(channel) = process.exception_channel()
        && let Some(delivery) = try_handler(&channel, report)
    {
        return delivery;
    }
    let mut job = Some(process.job().clone());
    while let Some(current) = job {
        if let Some(channel) = current.exception_channel()
            && let Some(delivery) = try_handler(&channel, report)
        {
            return delivery;
        }
        job = current.parent().cloned();
    }
    Delivery::Unhandled
}

/// 交给一个处理者；返回 `None` 表示交给下一级。
fn try_handler(channel: &Arc<Channel>, report: ExceptionReport) -> Option<Delivery> {
    let Ok((exception, waiter)) = Exception::create() else {
        return Some(Delivery::Unhandled);
    };
    let mut bytes = Vec::new();
    let mut handles = Vec::new();
    if bytes
        .try_reserve_exact(core::mem::size_of::<ExceptionReport>())
        .is_err()
        || handles.try_reserve_exact(1).is_err()
    {
        return Some(Delivery::Unhandled);
    }
    bytes.extend_from_slice(bytes_of(&report));
    handles.push(HandleEntry {
        object: Object::Exception(exception),
        rights: native_abi::rights::TRANSFER
            | native_abi::rights::READ
            | native_abi::rights::WRITE
            | native_abi::rights::INSPECT,
    });
    // 处理者已关闭或积压过多时交给下一级。
    channel.write(Message { bytes, handles }).ok()?;
    match waiter.wait() {
        Some(ExceptionResolution::Resume(state)) => Some(Delivery::Resume(state)),
        Some(ExceptionResolution::TryNext) => None,
        None => Some(Delivery::Killed),
    }
}
