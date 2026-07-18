/// @description 将 Sv39 virtual page number 分解为 root-to-leaf 三层索引。
pub(crate) fn indexes(virtual_page: usize) -> [usize; 3] {
    [
        (virtual_page >> 18) & 0x1ff,
        (virtual_page >> 9) & 0x1ff,
        virtual_page & 0x1ff,
    ]
}
