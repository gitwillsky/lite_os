//! @description Linux/arm64 `rt_sigframe` 的纯字节 codec 与 extension-chain validator。

use core::ops::Range;

pub(crate) const SIGNAL_FRAME_SIZE: usize = 4688;
/// @description Linux/arm64 `MINSIGSTKSZ`，为标准 frame 与 handler entry 保留余量。
pub(crate) const MIN_SIGNAL_STACK_SIZE: usize = 5120;
const SIGINFO_SIZE: usize = 128;
const UCONTEXT_OFFSET: usize = SIGINFO_SIZE;
const STACK_OFFSET: usize = UCONTEXT_OFFSET + 16;
const SIGNAL_MASK_OFFSET: usize = UCONTEXT_OFFSET + 40;
// sigcontext.__reserved 的 16-byte alignment 会把整个 uc_mcontext 提升为 16-byte 对齐；
// 168-byte ucontext prefix 后因此存在 8-byte ABI padding。
const SIGCONTEXT_OFFSET: usize = 304;
const REGS_OFFSET: usize = SIGCONTEXT_OFFSET + 8;
const SP_OFFSET: usize = REGS_OFFSET + 31 * 8;
const PC_OFFSET: usize = SP_OFFSET + 8;
const PSTATE_OFFSET: usize = PC_OFFSET + 8;
const RESERVED_OFFSET: usize = SIGCONTEXT_OFFSET + 288;
const RESERVED_SIZE: usize = 4096;
const FPSIMD_MAGIC: u32 = 0x4650_8001;
const FPSIMD_SIZE: u32 = 528;
const FPSIMD_STATE_OFFSET: usize = RESERVED_OFFSET + 8;
const TERMINATOR_OFFSET: usize = RESERVED_OFFSET + FPSIMD_SIZE as usize;

const _: () = {
    assert!(SIGCONTEXT_OFFSET == 304);
    assert!(SIGCONTEXT_OFFSET.is_multiple_of(16));
    assert!(RESERVED_OFFSET == 592);
    assert!(RESERVED_OFFSET.is_multiple_of(16));
    assert!(RESERVED_OFFSET + RESERVED_SIZE == SIGNAL_FRAME_SIZE);
    assert!((FPSIMD_STATE_OFFSET + 8).is_multiple_of(16));
    assert!(TERMINATOR_OFFSET + 8 <= SIGNAL_FRAME_SIZE);
};

/// @description Linux `stack_t` 中 signal frame 必须保存的 architecture-neutral 值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SignalStack {
    sp: usize,
    flags: i32,
    size: usize,
}

impl SignalStack {
    /// @description 构造待写入 `ucontext_t.uc_stack` 的快照。
    /// @param sp alternate stack base。
    /// @param flags Linux `SS_*` flags。
    /// @param size alternate stack byte size。
    /// @return owned stack snapshot。
    pub(crate) const fn new(sp: usize, flags: i32, size: usize) -> Self {
        Self { sp, flags, size }
    }

    /// @description 返回 alternate stack base。
    pub(crate) const fn sp(self) -> usize {
        self.sp
    }

    /// @description 返回 Linux `SS_*` flags。
    pub(crate) const fn flags(self) -> i32 {
        self.flags
    }

    /// @description 返回 alternate stack byte size。
    pub(crate) const fn size(self) -> usize {
        self.size
    }
}

/// @description 已完成全部 structural validation 的 arm64 integer signal state。
pub(super) struct DecodedSignalFrame {
    pub(super) registers: [usize; 31],
    pub(super) stack_pointer: usize,
    pub(super) program_counter: usize,
    pub(super) pstate: usize,
    pub(super) signal_mask: u64,
    pub(super) signal_stack: SignalStack,
}

/// @description 固定 4688-byte、16-byte aligned 的 Linux/arm64 `rt_sigframe` image。
#[repr(C, align(16))]
pub(crate) struct SignalFrame {
    bytes: [u8; SIGNAL_FRAME_SIZE],
}

impl SignalFrame {
    /// @description 构造包含唯一 FPSIMD record 与 terminator 的 signal frame。
    /// @param info 128-byte Linux `siginfo_t` image。
    /// @param signal_stack delivery 前的 alternate stack 状态。
    /// @param signal_mask delivery 前的 blocked signal mask。
    /// @param registers x0..x30。
    /// @param stack_pointer EL0 SP。
    /// @param program_counter EL0 PC。
    /// @param pstate EL0 PSTATE/SPSR image。
    /// @return FPSIMD payload 尚为零、可由唯一 capture asm 填充的 frame。
    pub(super) fn encode(
        info: [u8; SIGINFO_SIZE],
        signal_stack: SignalStack,
        signal_mask: u64,
        registers: [usize; 31],
        stack_pointer: usize,
        program_counter: usize,
        pstate: usize,
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
        for (index, register) in registers.into_iter().enumerate() {
            put_u64(&mut frame.bytes, REGS_OFFSET + index * 8, register as u64);
        }
        put_u64(&mut frame.bytes, SP_OFFSET, stack_pointer as u64);
        put_u64(&mut frame.bytes, PC_OFFSET, program_counter as u64);
        put_u64(&mut frame.bytes, PSTATE_OFFSET, pstate as u64);
        put_u32(&mut frame.bytes, RESERVED_OFFSET, FPSIMD_MAGIC);
        put_u32(&mut frame.bytes, RESERVED_OFFSET + 4, FPSIMD_SIZE);
        // zeroed frame already owns the required zero/zero terminator and trailing reserved bytes.
        frame
    }

    /// @description 构造供 user-copy 填充的零 frame。
    /// @return 全部 bytes 为零的 owned frame。
    pub(crate) const fn zeroed() -> Self {
        Self {
            bytes: [0; SIGNAL_FRAME_SIZE],
        }
    }

    /// @description 返回 frame 的 immutable user-copy bytes。
    pub(crate) const fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// @description 返回 frame 的 mutable user-copy bytes。
    pub(crate) fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// @description 返回唯一 FPSIMD payload 的 mutable capture address。
    /// @return pointer+8 为 16-byte aligned vregs，覆盖 fpsr/fpcr/vregs 的 pointer。
    pub(super) fn fpsimd_state_mut_ptr(&mut self) -> *mut u8 {
        // FPSIMD_STATE_OFFSET+8 has a compile-time 16-byte vregs alignment proof above.
        self.bytes.as_mut_ptr().wrapping_add(FPSIMD_STATE_OFFSET)
    }

    /// @description 返回经 validation 后唯一 FPSIMD payload 的 restore address。
    pub(super) fn fpsimd_state_ptr(&self) -> *const u8 {
        self.bytes.as_ptr().wrapping_add(FPSIMD_STATE_OFFSET)
    }

    /// @description 完整验证 ucontext prefix、FPSIMD chain 与 terminator 后解码 integer state。
    /// @return validated integer state；FP payload 保持在 frame 中供 asm restore。
    /// @errors unknown/duplicate/missing record、非法 size/alignment/terminator 或非零尾部。
    pub(super) fn decode(
        &self,
        user_address_end: usize,
    ) -> Result<DecodedSignalFrame, InvalidSignalFrame> {
        self.validate_context_chain()?;
        let mut registers = [0usize; 31];
        for (index, register) in registers.iter_mut().enumerate() {
            *register = get_u64(&self.bytes, REGS_OFFSET + index * 8) as usize;
        }
        let decoded = DecodedSignalFrame {
            registers,
            stack_pointer: get_u64(&self.bytes, SP_OFFSET) as usize,
            program_counter: get_u64(&self.bytes, PC_OFFSET) as usize,
            pstate: get_u64(&self.bytes, PSTATE_OFFSET) as usize,
            signal_mask: get_u64(&self.bytes, SIGNAL_MASK_OFFSET),
            signal_stack: SignalStack::new(
                get_u64(&self.bytes, STACK_OFFSET) as usize,
                get_u32(&self.bytes, STACK_OFFSET + 8) as i32,
                get_u64(&self.bytes, STACK_OFFSET + 16) as usize,
            ),
        };
        // This baseline publishes only NZCV across sigreturn. EL0 mode is therefore implicit zero;
        // accepting DAIF/PAN/UAO/BTYPE/SSBS/TCO without owning their full lifecycle would let a
        // user frame alter privileged or untracked execution controls at ERET.
        const USER_PSTATE_MASK: usize = 0xf000_0000;
        if decoded.pstate & !USER_PSTATE_MASK != 0
            || decoded.program_counter >= user_address_end
            || decoded.program_counter & 3 != 0
        {
            return Err(InvalidSignalFrame);
        }
        Ok(decoded)
    }

    fn validate_context_chain(&self) -> Result<(), InvalidSignalFrame> {
        let reserved = &self.bytes[RESERVED_OFFSET..RESERVED_OFFSET + RESERVED_SIZE];
        let mut offset = 0usize;
        let mut found_fpsimd = false;
        loop {
            let header = checked_range(offset, 8, reserved.len())?;
            let magic = get_u32(reserved, header.start);
            let size = get_u32(reserved, header.start + 4) as usize;
            if magic == 0 || size == 0 {
                if magic != 0 || size != 0 || !found_fpsimd {
                    return Err(InvalidSignalFrame);
                }
                if reserved[header.end..].iter().any(|byte| *byte != 0) {
                    return Err(InvalidSignalFrame);
                }
                return Ok(());
            }
            if offset & 0xf != 0 || size < 8 || size & 0xf != 0 {
                return Err(InvalidSignalFrame);
            }
            checked_range(offset, size, reserved.len())?;
            match magic {
                FPSIMD_MAGIC if size == FPSIMD_SIZE as usize && !found_fpsimd => {
                    found_fpsimd = true;
                }
                _ => return Err(InvalidSignalFrame),
            }
            offset = offset.checked_add(size).ok_or(InvalidSignalFrame)?;
        }
    }
}

/// @description 用户提供的 arm64 `rt_sigframe` validation failure。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidSignalFrame;

fn checked_range(
    start: usize,
    size: usize,
    limit: usize,
) -> Result<Range<usize>, InvalidSignalFrame> {
    let end = start.checked_add(size).ok_or(InvalidSignalFrame)?;
    if end > limit {
        return Err(InvalidSignalFrame);
    }
    Ok(start..end)
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

    fn frame() -> SignalFrame {
        SignalFrame::encode(
            [0x5a; 128],
            SignalStack::new(0x1000, 1, 0x2000),
            0x55aa,
            core::array::from_fn(|index| index + 1),
            0x3000,
            0x4000,
            0,
        )
    }

    #[test]
    fn layout_matches_linux_arm64_offsets_and_size() {
        let frame = frame();
        assert_eq!(frame.as_bytes().len(), 4688);
        assert_eq!(MIN_SIGNAL_STACK_SIZE, 5120);
        assert_eq!(UCONTEXT_OFFSET, 128);
        assert_eq!(STACK_OFFSET, 144);
        assert_eq!(SIGNAL_MASK_OFFSET, 168);
        assert_eq!(SIGCONTEXT_OFFSET, 304);
        assert_eq!(REGS_OFFSET, 312);
        assert_eq!(SP_OFFSET, 560);
        assert_eq!(PC_OFFSET, 568);
        assert_eq!(PSTATE_OFFSET, 576);
        assert_eq!(RESERVED_OFFSET, 592);
        assert_eq!(FPSIMD_STATE_OFFSET, 600);
        assert_eq!(TERMINATOR_OFFSET, 1120);
        assert_eq!(get_u64(frame.as_bytes(), SIGNAL_MASK_OFFSET), 0x55aa);
        assert_eq!(get_u64(frame.as_bytes(), REGS_OFFSET), 1);
        assert_eq!(get_u64(frame.as_bytes(), SP_OFFSET), 0x3000);
        assert_eq!(get_u64(frame.as_bytes(), PC_OFFSET), 0x4000);
        assert_eq!(get_u32(frame.as_bytes(), RESERVED_OFFSET), FPSIMD_MAGIC);
        assert_eq!(get_u32(frame.as_bytes(), RESERVED_OFFSET + 4), 528);
        assert_eq!(get_u64(frame.as_bytes(), TERMINATOR_OFFSET), 0);
    }

    #[test]
    fn decoder_rejects_unknown_duplicate_missing_and_malformed_records() {
        let mut unknown = frame();
        put_u32(&mut unknown.bytes, RESERVED_OFFSET, 0x1234);
        assert!(unknown.decode(1 << 38).is_err());

        let mut duplicate = frame();
        put_u32(&mut duplicate.bytes, TERMINATOR_OFFSET, FPSIMD_MAGIC);
        put_u32(&mut duplicate.bytes, TERMINATOR_OFFSET + 4, FPSIMD_SIZE);
        assert!(duplicate.decode(1 << 38).is_err());

        let mut missing = frame();
        put_u32(&mut missing.bytes, RESERVED_OFFSET, 0);
        put_u32(&mut missing.bytes, RESERVED_OFFSET + 4, 0);
        assert!(missing.decode(1 << 38).is_err());

        let mut malformed = frame();
        put_u32(&mut malformed.bytes, RESERVED_OFFSET + 4, FPSIMD_SIZE - 16);
        assert!(malformed.decode(1 << 38).is_err());

        let mut bad_terminator = frame();
        put_u32(&mut bad_terminator.bytes, TERMINATOR_OFFSET + 4, 16);
        assert!(bad_terminator.decode(1 << 38).is_err());
    }

    #[test]
    fn decoder_rejects_privileged_pstate_and_invalid_user_pc() {
        let mut privileged = frame();
        put_u64(&mut privileged.bytes, PSTATE_OFFSET, 0x5);
        assert!(privileged.decode(1 << 38).is_err());

        let mut daif = frame();
        put_u64(&mut daif.bytes, PSTATE_OFFSET, 1 << 7);
        assert!(daif.decode(1 << 38).is_err());

        let mut pan = frame();
        put_u64(&mut pan.bytes, PSTATE_OFFSET, 1 << 22);
        assert!(pan.decode(1 << 38).is_err());

        let mut nzcv = frame();
        put_u64(&mut nzcv.bytes, PSTATE_OFFSET, 0xa000_0000);
        assert!(nzcv.decode(1 << 38).is_ok());

        let mut unaligned = frame();
        put_u64(&mut unaligned.bytes, PC_OFFSET, 0x4002);
        assert!(unaligned.decode(1 << 38).is_err());

        let mut kernel = frame();
        put_u64(&mut kernel.bytes, PC_OFFSET, 1 << 38);
        assert!(kernel.decode(1 << 38).is_err());
    }
}
