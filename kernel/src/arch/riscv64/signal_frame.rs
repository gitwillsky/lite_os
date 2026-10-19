//! Linux/riscv64 `rt_sigframe` 的 architecture-owned byte-exact codec。

pub(crate) const SIGNAL_FRAME_SIZE: usize = 1088;
/// 保持既有 Linux/riscv64 `MINSIGSTKSZ` ABI。
pub(crate) const MIN_SIGNAL_STACK_SIZE: usize = 2048;
const SIGINFO_SIZE: usize = 128;
const UCONTEXT_OFFSET: usize = SIGINFO_SIZE;
const STACK_OFFSET: usize = UCONTEXT_OFFSET + 16;
const SIGNAL_MASK_OFFSET: usize = UCONTEXT_OFFSET + 40;
// Linux 的 Q/extension union 按 16 bytes 对齐，使 uc_mcontext 前有八个 padding bytes。
// 缺失该 padding 会让 musl ucontext_t 读取下一寄存器并在 sigreturn 恢复错误上下文。
const MACHINE_OFFSET: usize = UCONTEXT_OFFSET + 176;
const REGISTERS_OFFSET: usize = MACHINE_OFFSET;
const FLOATING_POINT_OFFSET: usize = REGISTERS_OFFSET + 32 * 8;

const _: () = {
    assert!(MACHINE_OFFSET.is_multiple_of(16));
    assert!(FLOATING_POINT_OFFSET + 528 == SIGNAL_FRAME_SIZE);
};

/// Linux `stack_t` 中 signal frame 必须保存的 architecture-neutral 值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SignalStack {
    sp: usize,
    flags: i32,
    size: usize,
}

impl SignalStack {
    /// 构造待写入 `ucontext_t.uc_stack` 的快照。
    pub(crate) const fn new(sp: usize, flags: i32, size: usize) -> Self {
        Self { sp, flags, size }
    }

    /// 返回 alternate stack base。
    pub(crate) const fn sp(self) -> usize {
        self.sp
    }

    /// 返回 Linux `SS_*` flags。
    pub(crate) const fn flags(self) -> i32 {
        self.flags
    }

    /// 返回 alternate stack byte size。
    pub(crate) const fn size(self) -> usize {
        self.size
    }
}

/// 不受 task module 解释的 Linux/RISC-V signal machine context。
#[derive(Clone, Copy)]
pub(super) struct SignalMachineContext {
    pub(super) registers: [usize; 32],
    pub(super) floating_point: [u8; 528],
}

/// 已解码的 RISC-V frame metadata 与 machine context。
pub(super) struct DecodedSignalFrame {
    pub(super) machine: SignalMachineContext,
    pub(super) signal_mask: u64,
    pub(super) signal_stack: SignalStack,
}

/// Linux/riscv64 的 1088-byte base rt_sigframe，包含标准 16-byte 对齐 padding。
#[repr(C, align(16))]
pub(crate) struct SignalFrame {
    bytes: [u8; SIGNAL_FRAME_SIZE],
}

impl SignalFrame {
    /// Linux/RISC-V 在 rt_sigreturn 中没有单独的用户 SP alignment 拒绝条件。
    pub(crate) fn valid_user_address(_address: usize) -> bool {
        true
    }

    /// Linux/RISC-V badframe 使用无 fault address 的 SI_KERNEL SIGSEGV。
    pub(crate) fn fault_address(_address: usize) -> Option<usize> {
        None
    }

    /// 任意 machine restore 失败仍使用 SI_KERNEL，而不暴露 frame/saved SP。
    pub(crate) fn restore_fault_address(&self) -> Option<usize> {
        None
    }
    /// 投影已完整 copyin 的 saved mask；不要求 machine/extension validation 成功。
    pub(crate) fn signal_mask(&self) -> u64 {
        get_u64(&self.bytes, SIGNAL_MASK_OFFSET)
    }
    /// 编码标准 RISC-V base frame；扩展 header 与 alignment padding 保持为零。
    pub(super) fn encode(
        info: [u8; SIGINFO_SIZE],
        signal_stack: SignalStack,
        signal_mask: u64,
        machine: &SignalMachineContext,
    ) -> Self {
        let mut frame = Self::zeroed();
        frame.bytes[..SIGINFO_SIZE].copy_from_slice(&info);
        put_u64(&mut frame.bytes, STACK_OFFSET, signal_stack.sp as u64);
        put_u32(
            &mut frame.bytes,
            STACK_OFFSET + 8,
            signal_stack.flags as u32,
        );
        put_u64(
            &mut frame.bytes,
            STACK_OFFSET + 16,
            signal_stack.size as u64,
        );
        put_u64(&mut frame.bytes, SIGNAL_MASK_OFFSET, signal_mask);
        for (index, register) in machine.registers.iter().enumerate() {
            put_u64(
                &mut frame.bytes,
                REGISTERS_OFFSET + index * 8,
                *register as u64,
            );
        }
        frame.bytes[FLOATING_POINT_OFFSET..].copy_from_slice(&machine.floating_point);
        frame
    }

    /// 构造供 user-copy 填充的零 frame。
    pub(crate) const fn zeroed() -> Self {
        Self {
            bytes: [0; SIGNAL_FRAME_SIZE],
        }
    }

    /// 返回 frame 的 immutable user-copy bytes。
    pub(crate) const fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 返回 frame 的 mutable user-copy bytes。
    pub(crate) fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// 解码标准 RISC-V base frame；extension validation 由 UserContext restore 完成。
    pub(super) fn decode(&self) -> DecodedSignalFrame {
        let mut registers = [0usize; 32];
        for (index, register) in registers.iter_mut().enumerate() {
            *register = get_u64(&self.bytes, REGISTERS_OFFSET + index * 8) as usize;
        }
        let mut floating_point = [0u8; 528];
        floating_point.copy_from_slice(&self.bytes[FLOATING_POINT_OFFSET..]);
        DecodedSignalFrame {
            machine: SignalMachineContext {
                registers,
                floating_point,
            },
            signal_mask: get_u64(&self.bytes, SIGNAL_MASK_OFFSET),
            signal_stack: SignalStack::new(
                get_u64(&self.bytes, STACK_OFFSET) as usize,
                get_u32(&self.bytes, STACK_OFFSET + 8) as i32,
                get_u64(&self.bytes, STACK_OFFSET + 16) as usize,
            ),
        }
    }
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed u32 field"),
    )
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed u64 field"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_matches_linux_riscv64_ucontext_alignment() {
        let mut floating_point = [0u8; 528];
        floating_point[0] = 0xa5;
        floating_point[527] = 0x5a;
        let machine = SignalMachineContext {
            registers: core::array::from_fn(|index| 0x100 + index),
            floating_point,
        };
        let frame = SignalFrame::encode(
            [0x3c; 128],
            SignalStack::new(0x2000, 1, 0x3000),
            0x55aa,
            &machine,
        );
        assert_eq!(frame.as_bytes().len(), 1088);
        assert_eq!(core::mem::align_of::<SignalFrame>(), 16);
        assert_eq!(MIN_SIGNAL_STACK_SIZE, 2048);
        assert_eq!(UCONTEXT_OFFSET, 128);
        assert_eq!(STACK_OFFSET, 144);
        assert_eq!(SIGNAL_MASK_OFFSET, 168);
        assert_eq!(MACHINE_OFFSET, 304);
        assert_eq!(REGISTERS_OFFSET, 304);
        assert_eq!(FLOATING_POINT_OFFSET, 560);
        assert_eq!(&frame.as_bytes()[296..304], &[0; 8]);
        assert_eq!(get_u64(frame.as_bytes(), SIGNAL_MASK_OFFSET), 0x55aa);
        assert_eq!(get_u64(frame.as_bytes(), REGISTERS_OFFSET), 0x100);
        assert_eq!(frame.as_bytes()[FLOATING_POINT_OFFSET], 0xa5);
        assert_eq!(frame.as_bytes()[1087], 0x5a);
        let decoded = frame.decode();
        assert_eq!(decoded.machine.registers, machine.registers);
        assert_eq!(decoded.machine.floating_point, machine.floating_point);
    }
}
