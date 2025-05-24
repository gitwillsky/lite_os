use riscv::register;

#[inline(always)]
pub(crate) fn hart_id() -> usize {
    register::mhartid::read()
}
