/// @description 内核 entropy facade；具体设备 adapter 只通过 drivers seam 提供字节。
pub(crate) fn fill(bytes: &mut [u8]) -> Result<(), ()> {
    crate::drivers::fill_entropy(bytes)
}
