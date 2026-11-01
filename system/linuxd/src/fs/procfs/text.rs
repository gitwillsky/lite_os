use alloc::vec::Vec;
use core::fmt::{self, Write};

use super::FileSystemError;

pub(super) struct ProcText(Vec<u8>);

impl ProcText {
    pub(super) const fn new() -> Self {
        Self(Vec::new())
    }

    /// 原生接口名是字节序列；不把合法非 UTF-8 名称替换或遗漏。
    pub(super) fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), FileSystemError> {
        self.0
            .try_reserve(bytes.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    pub(super) fn finish(self) -> Vec<u8> {
        self.0
    }
}

impl Write for ProcText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.try_reserve(text.len()).map_err(|_| fmt::Error)?;
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

pub(super) fn proc_text(arguments: fmt::Arguments<'_>) -> Result<Vec<u8>, FileSystemError> {
    let mut output = ProcText::new();
    output
        .write_fmt(arguments)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    Ok(output.finish())
}
