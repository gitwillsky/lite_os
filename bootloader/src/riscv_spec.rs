#![allow(unused)]

pub(crate) mod mie {
    use core::arch::asm;

    pub(crate) const SSIE: usize = 1 << 1;
    pub(crate) const VSSIE: usize = 1 << 2;
    pub(crate) const MSIE: usize = 1 << 3;
    pub(crate) const STIE: usize = 1 << 5;
    pub(crate) const VSTIE: usize = 1 << 6;
    pub(crate) const MTIE: usize = 1 << 7;
    pub(crate) const SEIE: usize = 1 << 9;
    pub(crate) const VSEIE: usize = 1 << 10;
    pub(crate) const MEIE: usize = 1 << 11;
    pub(crate) const SGEIE: usize = 1 << 12;

    #[inline(always)]
    pub(crate) fn write(bits: usize) {
        // SAFETY: firmware owns machine interrupt enable policy and executes this in M-mode.
        unsafe { asm!("csrw mie, {}", in(reg) bits, options(nomem)) };
    }
}

pub(crate) mod mstatus {
    use core::arch::asm;

    pub(crate) const SIE: usize = 1 << 1;
    pub(crate) const MIE: usize = 1 << 3;
    pub(crate) const SPIE: usize = 1 << 5;
    pub(crate) const MPIE: usize = 1 << 7;
    pub(crate) const SPP: usize = 1 << 8;
    pub(crate) const VS: usize = 3 << 9;
    pub(crate) const MPP: usize = 3 << 11;
    pub(crate) const FS: usize = 3 << 13;
    pub(crate) const XS: usize = 3 << 15;
    pub(crate) const MPRV: usize = 1 << 17;
    pub(crate) const SUM: usize = 1 << 18;
    pub(crate) const MXR: usize = 1 << 19;
    pub(crate) const TVM: usize = 1 << 20;
    pub(crate) const TW: usize = 1 << 21;
    pub(crate) const TSR: usize = 1 << 22;
    pub(crate) const UXL: usize = 3 << 32;
    pub(crate) const SXL: usize = 3 << 34;
    pub(crate) const SBE: usize = 1 << 36;
    pub(crate) const MBE: usize = 1 << 37;
    pub(crate) const SD: usize = 1 << 63;

    pub(crate) const MPP_MACHINE: usize = 3 << 11;
    pub(crate) const MPP_SUPERVISOR: usize = 1 << 11;
    pub(crate) const MPP_USER: usize = 0 << 11;

    pub(crate) fn update(f: impl FnOnce(&mut usize)) {
        let mut bits: usize;
        // SAFETY: M-mode CSR read has no memory effect and initializes the complete word.
        unsafe { asm!("csrr {}, mstatus", out(reg) bits, options(nomem)) };
        f(&mut bits);
        // SAFETY: firmware owns mstatus; caller closure intentionally selects replacement bits.
        unsafe { asm!("csrw mstatus, {}", in(reg) bits, options(nomem)) };
    }

    #[inline(always)]
    pub(crate) fn read() -> usize {
        let bits: usize;
        // SAFETY: M-mode CSR read has no memory effect and initializes the complete word.
        unsafe { asm!("csrr {}, mstatus", out(reg) bits, options(nomem)) };
        bits
    }
}

pub(crate) mod mepc {
    use core::arch::asm;

    #[inline(always)]
    pub(crate) fn next() {
        // SAFETY: trap handler owns mepc in M-mode; advancing one 32-bit ecall instruction is the
        // SBI return protocol and the assembly touches no memory.
        unsafe {
            asm!(
                "   csrr {0}, mepc
                    addi {0}, {0}, 4
                    csrw mepc, {0}
                ",
                out(reg) _,
                options(nomem),
            )
        }
    }

    #[inline(always)]
    pub(crate) fn read() -> usize {
        let bits: usize;
        // SAFETY: trap handler reads its M-mode exception PC without memory side effects.
        unsafe { asm!("csrr {}, mepc", out(reg) bits, options(nomem)) };
        bits
    }

    #[inline(always)]
    pub(crate) fn write(bits: usize) {
        // SAFETY: trap handler owns mepc and writes a validated supervisor resume address.
        unsafe { asm!("csrw mepc, {}", in(reg) bits, options(nomem)) };
    }
}
