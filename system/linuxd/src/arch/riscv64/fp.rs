//! Linux 程序浮点状态的显式保存与恢复。
//!
//! 服务代码按 `imac`（无浮点）ABI 编译、从不触碰浮点寄存器，因此在 restricted 退出后 f0-f31
//! 与 fcsr 仍是 Linux 程序的值。这里的例程是服务访问它们的唯一入口；首次执行浮点指令时
//! 内核按需为本线程启用浮点单元。

/// 一份完整的 D 扩展浮点状态。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FpState {
    pub(crate) f: [u64; 32],
    pub(crate) fcsr: u32,
}

core::arch::global_asm!(
    ".text",
    ".balign 4",
    ".option push",
    ".option arch, +d",
    // a0：33 个 64 位字（f0..f31 后接 fcsr）。
    ".global linuxd_fp_capture",
    "linuxd_fp_capture:",
    "fsd f0, 0(a0)",
    "fsd f1, 8(a0)",
    "fsd f2, 16(a0)",
    "fsd f3, 24(a0)",
    "fsd f4, 32(a0)",
    "fsd f5, 40(a0)",
    "fsd f6, 48(a0)",
    "fsd f7, 56(a0)",
    "fsd f8, 64(a0)",
    "fsd f9, 72(a0)",
    "fsd f10, 80(a0)",
    "fsd f11, 88(a0)",
    "fsd f12, 96(a0)",
    "fsd f13, 104(a0)",
    "fsd f14, 112(a0)",
    "fsd f15, 120(a0)",
    "fsd f16, 128(a0)",
    "fsd f17, 136(a0)",
    "fsd f18, 144(a0)",
    "fsd f19, 152(a0)",
    "fsd f20, 160(a0)",
    "fsd f21, 168(a0)",
    "fsd f22, 176(a0)",
    "fsd f23, 184(a0)",
    "fsd f24, 192(a0)",
    "fsd f25, 200(a0)",
    "fsd f26, 208(a0)",
    "fsd f27, 216(a0)",
    "fsd f28, 224(a0)",
    "fsd f29, 232(a0)",
    "fsd f30, 240(a0)",
    "fsd f31, 248(a0)",
    "frcsr t0",
    "sd t0, 256(a0)",
    "ret",
    ".global linuxd_fp_restore",
    "linuxd_fp_restore:",
    "fld f0, 0(a0)",
    "fld f1, 8(a0)",
    "fld f2, 16(a0)",
    "fld f3, 24(a0)",
    "fld f4, 32(a0)",
    "fld f5, 40(a0)",
    "fld f6, 48(a0)",
    "fld f7, 56(a0)",
    "fld f8, 64(a0)",
    "fld f9, 72(a0)",
    "fld f10, 80(a0)",
    "fld f11, 88(a0)",
    "fld f12, 96(a0)",
    "fld f13, 104(a0)",
    "fld f14, 112(a0)",
    "fld f15, 120(a0)",
    "fld f16, 128(a0)",
    "fld f17, 136(a0)",
    "fld f18, 144(a0)",
    "fld f19, 152(a0)",
    "fld f20, 160(a0)",
    "fld f21, 168(a0)",
    "fld f22, 176(a0)",
    "fld f23, 184(a0)",
    "fld f24, 192(a0)",
    "fld f25, 200(a0)",
    "fld f26, 208(a0)",
    "fld f27, 216(a0)",
    "fld f28, 224(a0)",
    "fld f29, 232(a0)",
    "fld f30, 240(a0)",
    "fld f31, 248(a0)",
    "ld t0, 256(a0)",
    "fscsr t0",
    "ret",
    ".option pop",
);

// SAFETY: 本模块汇编定义两个例程，唯一参数指向完整 FpState，调用点保活并保证对齐。
unsafe extern "C" {
    fn linuxd_fp_capture(words: *mut u64);
    fn linuxd_fp_restore(words: *const u64);
}

/// 捕获当前线程的硬件浮点状态。
pub(crate) fn capture() -> FpState {
    let mut words = [0u64; 33];
    // SAFETY: 缓冲区由本函数独占、8 字节对齐，例程只写它。
    unsafe { linuxd_fp_capture(words.as_mut_ptr()) };
    let mut state = FpState::default();
    state.f.copy_from_slice(&words[..32]);
    state.fcsr = words[32] as u32;
    state
}

/// 把浮点状态装入当前线程的硬件寄存器。
pub(crate) fn restore(state: &FpState) {
    let mut words = [0u64; 33];
    words[..32].copy_from_slice(&state.f);
    words[32] = u64::from(state.fcsr);
    // SAFETY: 缓冲区有效且对齐；例程只读它。
    unsafe { linuxd_fp_restore(words.as_ptr()) };
}
