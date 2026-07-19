use super::*;

const SS_ONSTACK: u32 = 1;
const SS_DISABLE: u32 = 2;
const SS_AUTODISARM: u32 = 1 << 31;
const SS_FLAG_BITS: u32 = SS_AUTODISARM;

/// @description Linux `stack_t` 的领域表示；不包含 RV64 ABI padding。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SignalStack {
    /// 用户提供的 alternate stack 最低地址。
    pub(crate) sp: usize,
    /// `SS_DISABLE/SS_ONSTACK/SS_AUTODISARM` 组合。
    pub(crate) flags: i32,
    /// alternate stack 可用字节数。
    pub(crate) size: usize,
}

/// @description alternate signal stack 更新失败的领域原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalStackError {
    /// 当前用户 SP 位于不可自动解除的 alternate stack，禁止替换。
    Active,
    /// flags 含 Linux 未定义的 mode 或 bit。
    InvalidFlags,
    /// enabled stack 小于编译期 architecture 的 Linux `MINSIGSTKSZ`。
    TooSmall,
}

/// @description Thread 独占的 alternate signal stack registration。
#[derive(Debug, Clone, Copy)]
pub(super) struct AlternateSignalStack {
    sp: usize,
    flags: u32,
    size: usize,
}

impl AlternateSignalStack {
    /// @description 创建 Linux 初始/exec 后的 disabled registration。
    ///
    /// @return `SS_DISABLE` 且地址、长度为零的状态。
    pub(super) const fn disabled() -> Self {
        Self {
            sp: 0,
            flags: SS_DISABLE,
            size: 0,
        }
    }

    fn contains_sp(&self, sp: usize) -> bool {
        if self.flags & SS_AUTODISARM != 0 || self.size == 0 {
            return false;
        }
        sp > self.sp && sp - self.sp <= self.size
    }

    fn mode_at(&self, sp: usize) -> u32 {
        if self.size == 0 {
            SS_DISABLE
        } else if self.contains_sp(sp) {
            SS_ONSTACK
        } else {
            0
        }
    }

    fn saved(&self) -> SignalStack {
        SignalStack {
            sp: self.sp,
            flags: self.flags as i32,
            size: self.size,
        }
    }

    fn snapshot(&self, sp: usize) -> SignalStack {
        SignalStack {
            sp: self.sp,
            flags: (self.mode_at(sp) | (self.flags & SS_FLAG_BITS)) as i32,
            size: self.size,
        }
    }

    fn replace(&mut self, sp: usize, replacement: SignalStack) -> Result<(), SignalStackError> {
        if self.contains_sp(sp) {
            return Err(SignalStackError::Active);
        }
        let flags = replacement.flags as u32;
        let mode = flags & !SS_FLAG_BITS;
        if !matches!(mode, 0 | SS_DISABLE | SS_ONSTACK) {
            return Err(SignalStackError::InvalidFlags);
        }
        if self.sp == replacement.sp && self.flags == flags && self.size == replacement.size {
            return Ok(());
        }
        if mode == SS_DISABLE {
            self.sp = 0;
            self.size = 0;
        } else {
            if replacement.size < crate::arch::context::MIN_SIGNAL_STACK_SIZE {
                return Err(SignalStackError::TooSmall);
            }
            self.sp = replacement.sp;
            self.size = replacement.size;
        }
        self.flags = flags;
        Ok(())
    }

    fn frame(
        &self,
        user_sp: usize,
        use_alternate: bool,
        frame_size: usize,
    ) -> Result<(usize, SignalStack), UserAccessError> {
        // 1. 已在普通 alternate stack 上时必须继续向下生长；越过 registration 底部会
        // 覆盖其他用户内存，必须在 user-copy 前按 Linux RISC-V get_sigframe 语义拒绝。
        if self.contains_sp(user_sp) {
            let bottom = user_sp
                .checked_sub(frame_size)
                .ok_or(UserAccessError::Fault)?;
            if !self.contains_sp(bottom) {
                return Err(UserAccessError::Fault);
            }
        }

        // 2. SA_ONSTACK 只在 enabled 且尚未位于 alternate stack 时切换；disabled stack
        // 继续使用普通 SP，否则未注册 altstack 的普通 handler 会被错误送往地址零。
        let top = if use_alternate && self.mode_at(user_sp) == 0 {
            self.sp
                .checked_add(self.size)
                .ok_or(UserAccessError::Fault)?
        } else {
            user_sp
        };
        let address = top.checked_sub(frame_size).ok_or(UserAccessError::Fault)? & !0xf;
        Ok((address, self.saved()))
    }
}

impl TaskControlBlock {
    /// @description 查询并可选替换当前 Thread 的 alternate signal stack。
    ///
    /// @param replacement 新 registration；`None` 只查询。
    /// @return 调用前 registration，并按当前用户 SP 动态投影 `SS_ONSTACK`。
    /// @errors active stack、非法 flags 或 enabled stack 过小时返回领域错误。
    pub(crate) fn signal_stack(
        &self,
        replacement: Option<SignalStack>,
    ) -> Result<SignalStack, SignalStackError> {
        let user_sp = self.user_stack_pointer();
        let mut state = self.thread.alternate_signal_stack.lock();
        let old = state.snapshot(user_sp);
        if let Some(replacement) = replacement {
            state.replace(user_sp, replacement)?;
        }
        Ok(old)
    }

    /// @description 为一次 architecture-owned signal frame 选择栈并保存原 registration。
    ///
    /// @param user_sp signal 前用户 SP。
    /// @param use_alternate disposition 是否含 `SA_ONSTACK`。
    /// @param frame_size 编译期 architecture 的固定 rt frame 字节数。
    /// @return 16-byte 对齐的 frame 地址与 `ucontext.uc_stack` 快照。
    /// @errors 地址算术溢出或 nested frame 越过 altstack 底部时返回 Fault。
    pub(super) fn signal_frame_stack(
        &self,
        user_sp: usize,
        use_alternate: bool,
        frame_size: usize,
    ) -> Result<(usize, SignalStack), UserAccessError> {
        self.thread
            .alternate_signal_stack
            .lock()
            .frame(user_sp, use_alternate, frame_size)
    }

    /// @description 在 signal frame 成功写入后提交 `SS_AUTODISARM`。
    ///
    /// @return 无返回值；普通 registration 保持不变。
    pub(super) fn commit_signal_stack_delivery(&self) {
        let mut state = self.thread.alternate_signal_stack.lock();
        // SS_AUTODISARM 只在 frame 已完整写入后消费；提前清除会让 copyout fault 永久丢失
        // registration，缺失该分支则 swapcontext 离开 handler 后可能复用已失效的 altstack。
        if state.flags & SS_AUTODISARM != 0 {
            *state = AlternateSignalStack::disabled();
        }
    }

    /// @description 按 sigreturn 已恢复的 SP 尝试恢复 `ucontext.uc_stack`。
    ///
    /// @param restored_sp signal 前用户 SP；用于动态判断原 registration 是否 active。
    /// @param saved 用户 frame 中的 registration。
    /// @return 无返回值；Linux 要求压平 registration validation error。
    pub(super) fn restore_signal_stack(&self, restored_sp: usize, saved: SignalStack) {
        // Linux sigreturn 只把读取 ucontext 的 EFAULT 视为坏 frame；registration 本身的
        // EPERM/EINVAL/ENOMEM 被压平，避免用户修改 uc_stack 使已恢复的寄存器失效。
        let _ = self
            .thread
            .alternate_signal_stack
            .lock()
            .replace(restored_sp, saved);
    }

    /// @description exec commit 时清除 calling Thread 的 alternate signal stack。
    ///
    /// @return 无返回值。
    pub(super) fn reset_signal_stack_for_exec(&self) {
        *self.thread.alternate_signal_stack.lock() = AlternateSignalStack::disabled();
    }
}
