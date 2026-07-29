//! Desktop-owned global accelerator table.

use crate::{
    MAX_ACCELERATORS,
    codec::{FrameWriter, MessageKind, PayloadReader},
};

/// One fixed physical chord: an exact modifier mask plus one evdev key code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcceleratorChord {
    /// Required Shift/Ctrl/Alt/Super mask, matched exactly: an extra held
    /// modifier prevents the match.
    pub modifiers: u32,
    /// Linux evdev key code completing the chord.
    pub code: u32,
}

/// Desktop message atomically replacing the whole global accelerator table.
///
/// Only the desktop connection may send it; window policy and the action
/// bound to each chord stay outside the compositor.
#[derive(Clone, Copy, Debug)]
pub struct AcceleratorSet<'a> {
    /// Complete chord list; at most [`MAX_ACCELERATORS`] entries. An empty
    /// list clears the table.
    pub chords: &'a [AcceleratorChord],
}

impl AcceleratorSet<'_> {
    /// Encodes one atomic table replacement.
    ///
    /// # Parameters
    ///
    /// - `bytes`: Caller-owned bounded frame storage.
    ///
    /// # Returns
    ///
    /// The complete frame, or `None` when the table exceeds
    /// [`MAX_ACCELERATORS`] or storage is too small.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        if self.chords.len() > MAX_ACCELERATORS {
            return None;
        }
        let mut writer = FrameWriter::new(bytes, MessageKind::AcceleratorSet)?;
        writer.u32(u32::try_from(self.chords.len()).ok()?)?;
        for chord in self.chords {
            writer.u32(chord.modifiers)?;
            writer.u32(chord.code)?;
        }
        writer.finish()
    }

    /// Parses and strictly validates one table payload.
    ///
    /// # Parameters
    ///
    /// - `payload`: Borrowed frame payload after the header.
    ///
    /// # Returns
    ///
    /// An exact-size iterator over the decoded chords, or `None` when the
    /// count exceeds [`MAX_ACCELERATORS`] or the payload is truncated or
    /// overlong.
    pub fn parse(payload: &[u8]) -> Option<AcceleratorChordIterator<'_>> {
        let mut reader = PayloadReader::new(payload);
        let count = reader.u32()? as usize;
        if count > MAX_ACCELERATORS {
            return None;
        }
        let bytes = reader.bytes(count.checked_mul(8)?)?;
        reader.finish()?;
        Some(AcceleratorChordIterator {
            reader: PayloadReader::new(bytes),
            remaining: count,
        })
    }
}

/// Exact-size iterator over wire-decoded chords.
///
/// The payload length was validated as exactly `count * 8` bytes, so decoding
/// cannot fail mid-iteration.
pub struct AcceleratorChordIterator<'a> {
    reader: PayloadReader<'a>,
    remaining: usize,
}

impl Iterator for AcceleratorChordIterator<'_> {
    type Item = AcceleratorChord;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let modifiers = self.reader.u32()?;
        let code = self.reader.u32()?;
        Some(AcceleratorChord { modifiers, code })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for AcceleratorChordIterator<'_> {}
