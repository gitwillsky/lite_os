const PAGE_SHIFT: usize = 12;
const TLBI_VIRTUAL_PAGE_MASK: u64 = (1u64 << 44) - 1;

/// Split a 39-bit virtual page number into root-to-leaf 9-bit indexes.
pub(crate) fn indexes(virtual_page: usize) -> [usize; 3] {
    [
        (virtual_page >> 18) & 0x1ff,
        (virtual_page >> 9) & 0x1ff,
        virtual_page & 0x1ff,
    ]
}

/// Encode the architected VA[55:12] operand used by `TLBI VAAE1*`.
///
/// Canonical high-half sign-extension bits above VA[55] are RES0 in the operand and must not be
/// forwarded to the instruction. The low page offset is discarded by the right shift.
pub(crate) const fn tlbi_all_asid_operand(virtual_address: usize) -> u64 {
    ((virtual_address as u64) >> PAGE_SHIFT) & TLBI_VIRTUAL_PAGE_MASK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_half_tlbi_operand_contains_only_va_55_through_12() {
        let address = 0xffff_ffc0_1234_5000usize;
        let operand = tlbi_all_asid_operand(address);
        assert_eq!(operand, 0x0fff_fc01_2345);
        assert_eq!(operand & !TLBI_VIRTUAL_PAGE_MASK, 0);
    }

    #[test]
    fn tlbi_operand_discards_page_offset_and_canonical_sign_extension() {
        assert_eq!(
            tlbi_all_asid_operand(0xffff_ffc0_1234_5abc),
            tlbi_all_asid_operand(0xffff_ffc0_1234_5000)
        );
    }
}
