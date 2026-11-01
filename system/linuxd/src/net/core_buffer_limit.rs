//! 全局网络 buffer 调优项的不可变 selector；实际值仅归 core_buffers owner。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CoreBufferLimit {
    SendDefault,
    ReceiveDefault,
    SendMaximum,
    ReceiveMaximum,
}
impl CoreBufferLimit {
    pub(crate) const ALL: [Self; 4] = [
        Self::SendDefault,
        Self::ReceiveDefault,
        Self::SendMaximum,
        Self::ReceiveMaximum,
    ];
    pub(super) fn index(self) -> usize {
        match self {
            Self::SendDefault => 0,
            Self::ReceiveDefault => 1,
            Self::SendMaximum => 2,
            Self::ReceiveMaximum => 3,
        }
    }
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            Self::SendDefault => b"wmem_default",
            Self::ReceiveDefault => b"rmem_default",
            Self::SendMaximum => b"wmem_max",
            Self::ReceiveMaximum => b"rmem_max",
        }
    }
}
