//! restricted mode：在同一线程上运行受限代码，并在其 syscall、未解决异常或 kick 时回到调用点。
//!
//! 内核语义（`RESTRICTED_ENTER`）：退出时 normal mode 寄存器恢复为进入时的值，PC 置为 `vector`，
//! 第一个参数为 `context`，第二个参数为原因码。[`enter`] 用一段汇编把这一跳转包装成普通函数返回：
//! 进入前把 callee-saved 寄存器压栈并以栈指针作为 `context`，`vector` 处据此恢复并返回原因码。

use native_abi::{RestrictedState, Status, call};

use crate::{Handle, Vmar, Vmo};

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
core::arch::global_asm!(
    ".text",
    ".global native_restricted_enter",
    ".type native_restricted_enter, %function",
    "native_restricted_enter:",
    "sub sp, sp, #112",
    "stp x19, x20, [sp, #0]",
    "stp x21, x22, [sp, #16]",
    "stp x23, x24, [sp, #32]",
    "stp x25, x26, [sp, #48]",
    "stp x27, x28, [sp, #64]",
    "stp x29, x30, [sp, #80]",
    "adr x0, 1f",
    "mov x1, sp",
    "mov x8, #{enter}",
    "svc #0",
    // 只有进入失败才回到这里：x0 是负的状态码。
    "b 2f",
    "1:",
    "mov sp, x0",
    "mov x0, x1",
    "2:",
    "ldp x19, x20, [sp, #0]",
    "ldp x21, x22, [sp, #16]",
    "ldp x23, x24, [sp, #32]",
    "ldp x25, x26, [sp, #48]",
    "ldp x27, x28, [sp, #64]",
    "ldp x29, x30, [sp, #80]",
    "add sp, sp, #112",
    "ret",
    enter = const call::RESTRICTED_ENTER,
);

#[cfg(all(target_os = "none", target_arch = "riscv64"))]
core::arch::global_asm!(
    ".text",
    ".global native_restricted_enter",
    ".type native_restricted_enter, @function",
    "native_restricted_enter:",
    "addi sp, sp, -112",
    "sd ra, 0(sp)",
    "sd s0, 8(sp)",
    "sd s1, 16(sp)",
    "sd s2, 24(sp)",
    "sd s3, 32(sp)",
    "sd s4, 40(sp)",
    "sd s5, 48(sp)",
    "sd s6, 56(sp)",
    "sd s7, 64(sp)",
    "sd s8, 72(sp)",
    "sd s9, 80(sp)",
    "sd s10, 88(sp)",
    "sd s11, 96(sp)",
    "sd gp, 104(sp)",
    "lla a0, 1f",
    "mv a1, sp",
    "li a7, {enter}",
    "ecall",
    // 只有进入失败才回到这里：a0 是负的状态码。
    "j 2f",
    "1:",
    "mv sp, a0",
    "mv a0, a1",
    "2:",
    "ld ra, 0(sp)",
    "ld s0, 8(sp)",
    "ld s1, 16(sp)",
    "ld s2, 24(sp)",
    "ld s3, 32(sp)",
    "ld s4, 40(sp)",
    "ld s5, 48(sp)",
    "ld s6, 56(sp)",
    "ld s7, 64(sp)",
    "ld s8, 72(sp)",
    "ld s9, 80(sp)",
    "ld s10, 88(sp)",
    "ld s11, 96(sp)",
    "ld gp, 104(sp)",
    "addi sp, sp, 112",
    "ret",
    enter = const call::RESTRICTED_ENTER,
);

// SAFETY: 上方汇编以本平台 C 调用约定保存寄存器；无参数，返回 native Status。
unsafe extern "C" {
    fn native_restricted_enter() -> i64;
}

/// restricted 代码退出的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// 执行了 syscall 指令；状态中的 PC 已指向其后。
    Syscall,
    /// 发生地址空间无法解决的异常；见状态中的异常描述。
    Exception,
    /// 线程被 kick。
    Kick,
}

/// 调用线程绑定的 restricted 状态：映射在 `vmar` 中的 [`RestrictedState`]。
pub struct BoundState {
    _vmo: Vmo,
    vmar: &'static Vmar,
    address: usize,
}

impl BoundState {
    /// 为调用线程绑定 restricted 状态，并映射到 `vmar`。
    ///
    /// # Errors
    ///
    /// 已绑定或内存不足。
    pub fn bind(vmar: &'static Vmar) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        crate::syscall::check(unsafe {
            crate::syscall::invoke(call::RESTRICTED_BIND_STATE, &[(&raw mut out) as usize])
        })?;
        // SAFETY: 成功时内核写入的新句柄由调用者独占。
        let vmo = Vmo::from_handle(unsafe { Handle::from_raw(out) });
        let size = core::mem::size_of::<RestrictedState>().next_multiple_of(native_abi::PAGE_SIZE);
        let address = match vmar.map(
            native_abi::vm::READ | native_abi::vm::WRITE,
            0,
            &vmo,
            0,
            size,
        ) {
            Ok(address) => address,
            Err(status) => {
                // SAFETY: 无指针参数；绑定刚建立，解除失败没有可恢复动作。
                let _ = unsafe { crate::syscall::invoke(call::RESTRICTED_UNBIND_STATE, &[]) };
                return Err(status);
            }
        };
        Ok(Self {
            _vmo: vmo,
            vmar,
            address,
        })
    }

    /// 寄存器与异常状态。
    pub fn state(&mut self) -> &mut RestrictedState {
        // SAFETY: 映射在本对象存活期间有效且只由所属线程访问（内核只在该线程进入/退出时读写）。
        unsafe { &mut *(self.address as *mut RestrictedState) }
    }

    /// 以状态中的寄存器进入 restricted mode，返回退出原因。
    ///
    /// # Errors
    ///
    /// `ERR_BAD_STATE`（未绑定或调用线程不属于共享进程成员）、`ERR_INVALID_ARGS`（寄存器非法）。
    pub fn enter(&mut self) -> Result<Exit, Status> {
        // SAFETY: 汇编遵守 C ABI：callee-saved 寄存器与栈在返回时恢复。
        let result = unsafe { native_restricted_enter() };
        match result {
            0 => Ok(Exit::Syscall),
            1 => Ok(Exit::Exception),
            2 => Ok(Exit::Kick),
            status if status < 0 => Err(status as Status),
            _ => Err(native_abi::ERR_INTERNAL),
        }
    }
}

impl Drop for BoundState {
    /// 只解除状态映射：绑定属于创建它的线程，内核在该线程退出时释放；析构可能发生在其他线程上
    /// （所有者对象随最后一个引用释放），在这里解除绑定会作用于错误的线程。
    fn drop(&mut self) {
        let size = core::mem::size_of::<RestrictedState>().next_multiple_of(native_abi::PAGE_SIZE);
        // SAFETY: 映射只由本对象持有，状态引用不能比本对象存活更久。
        let _ = unsafe { self.vmar.unmap(self.address, size) };
    }
}

// SAFETY: 映射地址与句柄可在线程间移动；状态只由绑定线程在进入/退出 restricted mode 的间隙访问。
unsafe impl Send for BoundState {}
