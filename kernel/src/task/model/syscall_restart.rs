//! Thread 的唯一 syscall 重放记录；raw errno 不得参与 signal/trap 决策。

/// Linux ERESTARTSYS 与 ERESTARTNOHAND 的语义；捕获 handler 时只有前者允许 SA_RESTART。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyscallRestartPolicy {
    WithRestartHandler,
    WithoutHandler,
    /// ERESTART_RESTARTBLOCK：捕获 handler 时 EINTR，无 handler 改调原生 restart_syscall。
    RestartBlock,
}

/// 已推进 PC 的一次 syscall 输入；只存于实际 Thread，消费后不能重放第二次。
#[derive(Debug, Clone, Copy)]
pub(super) struct SyscallRestart {
    pub(super) syscall_id: usize,
    pub(super) args: [usize; 6],
    pub(super) syscall_pc: usize,
    pub(super) policy: SyscallRestartPolicy,
}
impl SyscallRestart {
    /// 无捕获 handler（包括 stop/ignored）重启；捕获时按实际 Linux restart policy 决定。
    pub(super) fn resolve(mut self, handler_flags: Option<usize>) -> Option<Self> {
        const SA_RESTART: usize = 0x1000_0000;
        let restart = handler_flags.is_none_or(|flags| {
            self.policy == SyscallRestartPolicy::WithRestartHandler && flags & SA_RESTART != 0
        });
        if restart && self.policy == SyscallRestartPolicy::RestartBlock {
            self.syscall_id = syscall_abi::SYSCALL_RESTART_SYSCALL;
        }
        restart.then_some(self)
    }
}

/// 已导入并冻结期限的实际阻塞操作；重启不再次解码相对时间或用户 timeout pointer。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyscallRestartBlock {
    Futex {
        address: usize,
        expected: u32,
        private: bool,
        deadline: u64,
        bitset: u32,
    },
    Sleep {
        deadline: u64,
        remaining: usize,
    },
}

/// 一个 Thread 的 syscall instruction 与 restart block 生命周期；同锁交付、消费和清理。
#[derive(Default, Debug)]
pub(super) struct SyscallRestartState {
    instruction: Option<SyscallRestart>,
    block: Option<SyscallRestartBlock>,
}
impl SyscallRestartState {
    pub(super) fn arm(&mut self, instruction: SyscallRestart) {
        assert!(self.instruction.is_none(), "syscall restart armed twice");
        if instruction.policy == SyscallRestartPolicy::RestartBlock {
            assert!(
                self.block.is_some(),
                "restart instruction requires an actual block"
            );
        }
        self.instruction = Some(instruction);
    }
    pub(super) fn set_block(&mut self, block: SyscallRestartBlock) {
        self.block = Some(block);
    }
    pub(super) fn take_block(&mut self) -> Option<SyscallRestartBlock> {
        self.block.take()
    }
    pub(super) fn resolve(&mut self, handler_flags: Option<usize>) -> Option<SyscallRestart> {
        self.instruction
            .take()
            .and_then(|instruction| instruction.resolve(handler_flags))
    }
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}
