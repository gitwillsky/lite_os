//! restricted mode：在同一线程内运行不可信代码，任何 syscall、未解决异常或 kick 都退回 normal mode。
//!
//! 1. `bind_state` 为线程创建状态 VMO（布局为 [`RestrictedState`]）；调用者映射它读写寄存器。
//! 2. `enter` 保存当前 normal mode 寄存器（PC 已越过 syscall 指令），从状态 VMO 加载 restricted 寄存器，
//!    直接返回到 restricted 代码；该调用不写回状态码。
//! 3. [`exit`] 把 restricted 寄存器（与异常描述）写回状态 VMO，恢复进入时的 normal mode 寄存器，
//!    把 PC 置为 `vector`，前两个参数为 `context` 与原因码。
//!
//! 浮点寄存器不在状态中：两种模式共享同一套浮点状态，normal mode 代码负责自行保存。

use alloc::sync::Arc;

use native_abi::{ExceptionReport, RestrictedState, Status, rights};

use super::{Call, Outcome, bytes_of};
use crate::object::{Mode, Object, RestrictedEntry};
use crate::vm::{Vmo, VmoOptions};

pub(super) fn bind_state(call: &Call<'_>) -> Result<(), Status> {
    let size = core::mem::size_of::<RestrictedState>().next_multiple_of(native_abi::PAGE_SIZE);
    let vmo = Vmo::create(size as u64, VmoOptions::default()).map_err(crate::object::vm_status)?;
    let handle = call.process().handles().insert(crate::object::HandleEntry {
        object: Object::Vmo(vmo.clone()),
        rights: rights::READ | rights::WRITE | rights::MAP | rights::DUPLICATE | rights::INSPECT,
    })?;
    if let Err(status) = call.write(call.arg(0), &handle) {
        drop(call.process().handles().remove(handle));
        return Err(status);
    }
    call.owner.bind_restricted_state(vmo);
    Ok(())
}

pub(super) fn unbind_state(call: &Call<'_>) -> Result<(), Status> {
    call.owner.unbind_restricted_state();
    Ok(())
}

fn read_state(vmo: &Arc<Vmo>) -> Result<RestrictedState, Status> {
    let mut state = RestrictedState::default();
    // SAFETY: RestrictedState 是 Plain；按字节写入任意内容都是合法值。
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(
            (&mut state as *mut RestrictedState).cast::<u8>(),
            core::mem::size_of::<RestrictedState>(),
        )
    };
    vmo.read(0, bytes).map_err(crate::object::vm_status)?;
    Ok(state)
}

pub(super) fn enter(call: &Call<'_>) -> Outcome {
    let Some(vmo) = call.owner.restricted_state() else {
        return Outcome::Return(native_abi::ERR_BAD_STATE);
    };
    let vector = call.arg(0);
    let context = call.arg(1);
    if call.thread.take_kick() {
        // 进入前已有 kick：立即以 KICK 返回 vector，不进入 restricted mode。
        call.thread.with_user_context(|registers| {
            registers.set_program_counter(vector);
            registers.set_entry_arguments(context, native_abi::restricted::REASON_KICK as usize);
        });
        return Outcome::Preserve;
    }
    let state = match read_state(&vmo) {
        Ok(state) => state,
        Err(status) => return Outcome::Return(status),
    };
    let normal = call.thread.with_user_context(|registers| registers.save_registers());
    let loaded = call
        .thread
        .with_user_context(|registers| registers.load_registers(&state.registers));
    if loaded.is_err() {
        return Outcome::Return(native_abi::ERR_INVALID_ARGS);
    }
    call.owner.set_mode(Mode::Restricted(RestrictedEntry {
        vector,
        context,
        normal,
    }));
    call.thread.set_restricted_accounting(true);
    Outcome::Preserve
}

pub(super) fn kick(call: &Call<'_>) -> Result<(), Status> {
    let thread = call.thread_handle(call.arg_handle(0), rights::MANAGE)?;
    match thread.execution() {
        Some(execution) => {
            execution.kick();
            Ok(())
        }
        None => Err(native_abi::ERR_BAD_STATE),
    }
}

/// 退回 normal mode。状态 VMO 写入失败（已解除绑定）时线程无法再交出 restricted 状态，必须终止。
///
/// # Returns
/// 成功返回 `true`；状态无法写回时返回 `false`，调用者让线程退出。
pub(super) fn exit(
    owner: &crate::object::Thread,
    thread: &Arc<crate::thread::Thread>,
    entry: RestrictedEntry,
    reason: u64,
    exception: Option<ExceptionReport>,
) -> bool {
    let registers = thread.with_user_context(|context| context.save_registers());
    owner.set_mode(Mode::Normal);
    thread.set_restricted_accounting(false);
    let state = RestrictedState {
        registers,
        exception: exception.unwrap_or_default(),
    };
    let written = owner
        .restricted_state()
        .is_some_and(|vmo| vmo.write(0, bytes_of(&state)).is_ok());
    thread.with_user_context(|context| {
        context
            .load_registers(&entry.normal)
            .expect("saved normal-mode registers are always valid");
        context.set_program_counter(entry.vector);
        context.set_entry_arguments(entry.context, reason as usize);
    });
    written
}
