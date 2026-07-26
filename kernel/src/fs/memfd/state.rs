use alloc::vec::Vec;

pub(super) const F_SEAL_SEAL: u32 = 0x0001;
pub(super) const F_SEAL_SHRINK: u32 = 0x0002;
pub(super) const F_SEAL_GROW: u32 = 0x0004;
pub(super) const F_SEAL_WRITE: u32 = 0x0008;
pub(super) const F_SEAL_FUTURE_WRITE: u32 = 0x0010;
const SUPPORTED_SEALS: u32 = F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW;
const KNOWN_SEALS: u32 = SUPPORTED_SEALS | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MemFileStateError {
    InvalidOperation,
    OutOfMemory,
    PermissionDenied,
}

/// Anonymous bytes 与 seal mask 的单一线性化 owner。
pub(super) struct MemFileState {
    bytes: Vec<u8>,
    seals: u32,
}

impl MemFileState {
    pub(super) fn new(allow_sealing: bool) -> Self {
        Self {
            bytes: Vec::new(),
            seals: if allow_sealing { 0 } else { F_SEAL_SEAL },
        }
    }

    pub(super) fn add_seals(&mut self, seals: u32) -> Result<u32, MemFileStateError> {
        if seals & !KNOWN_SEALS != 0 || seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
            return Err(MemFileStateError::InvalidOperation);
        }
        if self.seals & F_SEAL_SEAL != 0 {
            return Err(MemFileStateError::PermissionDenied);
        }
        self.seals |= seals;
        Ok(self.seals)
    }

    pub(super) fn seals(&self) -> u32 {
        self.seals
    }

    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(super) fn read(&self, offset: usize, output: &mut [u8]) -> usize {
        let available = self.bytes.len().saturating_sub(offset).min(output.len());
        if available != 0 {
            output[..available].copy_from_slice(&self.bytes[offset..offset + available]);
        }
        available
    }

    pub(super) fn write(
        &mut self,
        offset: usize,
        input: &[u8],
    ) -> Result<usize, MemFileStateError> {
        if input.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(input.len())
            .ok_or(MemFileStateError::InvalidOperation)?;
        if self.seals & F_SEAL_GROW != 0 && end > self.bytes.len() {
            return Err(MemFileStateError::PermissionDenied);
        }
        if end > self.bytes.len() {
            self.bytes
                .try_reserve_exact(end - self.bytes.len())
                .map_err(|_| MemFileStateError::OutOfMemory)?;
            self.bytes.resize(end, 0);
        }
        self.bytes[offset..end].copy_from_slice(input);
        Ok(input.len())
    }

    pub(super) fn truncate(&mut self, size: usize) -> Result<(), MemFileStateError> {
        if size < self.bytes.len() && self.seals & F_SEAL_SHRINK != 0
            || size > self.bytes.len() && self.seals & F_SEAL_GROW != 0
        {
            return Err(MemFileStateError::PermissionDenied);
        }
        if size > self.bytes.len() {
            self.bytes
                .try_reserve_exact(size - self.bytes.len())
                .map_err(|_| MemFileStateError::OutOfMemory)?;
        }
        self.bytes.resize(size, 0);
        Ok(())
    }
}
