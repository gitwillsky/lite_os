use alloc::sync::Arc;
pub(crate) enum OpenFileKind {
    Socket(Arc<crate::socket::Socket>),
    Other,
}
pub(crate) struct OpenFileDescription {
    pub(crate) kind: OpenFileKind,
}
#[path = "../../../../kernel/src/file/unix_passed.rs"]
mod unix_passed;

impl Drop for OpenFileDescription {
    fn drop(&mut self) {
        if let OpenFileKind::Socket(socket) = &self.kind {
            socket.close_description();
        }
    }
}

#[path = "../../../../kernel/src/file/indexed_slots.rs"]
mod indexed_slots;
pub(crate) const MAX_FILE_DESCRIPTORS: usize = indexed_slots::MAX_FILE_DESCRIPTORS;
