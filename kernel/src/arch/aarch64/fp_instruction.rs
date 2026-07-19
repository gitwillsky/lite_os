/// Determine whether an AArch64 instruction belongs to FP/Advanced SIMD.
///
/// AArch64 does not use LiteOS's RISC-V lazy-FP trap protocol: FP/ASIMD is enabled for EL0 and its
/// state is eagerly represented by the architecture context. Returning false prevents an unknown
/// instruction from being incorrectly consumed as a first-use trap.
pub(crate) fn is_floating_point_instruction_at(
    _address: usize,
    _read_halfword: impl FnMut(usize, &mut [u8]) -> bool,
) -> bool {
    false
}
