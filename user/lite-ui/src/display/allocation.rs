//! Synchronous compositor buffer allocation and interleaved event routing.

use std::io;

use display_proto::{
    BufferAlloc, BufferAllocated, BufferRelease, MAX_MESSAGE, MessageKind, Size, parse_frame,
    recv_frame_blocking, send_message,
};

use super::{
    Buffer, Display, invalid,
    wire::{WireEvent, parse_event},
};

impl Display {
    /// Allocates `count` fresh compositor buffers for one configure size.
    ///
    /// The wait loop keeps applying retirement releases that overtake the
    /// allocation response: reconfigure-time cleanup on the compositor sends
    /// exactly those before answering, and flip-driven releases may land here
    /// on any later top-up.
    pub(super) fn allocate(&mut self, count: u32, physical: Size) -> io::Result<()> {
        let mut bytes = [0u8; 128];
        let request = BufferAlloc {
            request_id: 1,
            size: physical,
            count,
        }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("buffer request encoding failed"))?;
        send_message(&self.stream, request)?;
        let mut input = [0u8; MAX_MESSAGE];
        let allocated = loop {
            let (length, fd) = recv_frame_blocking(&self.stream, &mut input)?;
            if fd.is_some() {
                return Err(invalid("buffer response carried a descriptor"));
            }
            let frame =
                parse_frame(&input[..length]).ok_or_else(|| invalid("invalid display event"))?;
            match frame.kind() {
                MessageKind::BufferAllocated => {
                    break BufferAllocated::parse(frame.payload())
                        .filter(|response| {
                            response.request_id == 1
                                && response.error == 0
                                && response.count == count
                        })
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::OutOfMemory, "buffer request rejected")
                        })?;
                }
                MessageKind::BufferRelease => {
                    let id = BufferRelease::parse(frame.payload())
                        .ok_or_else(|| invalid("invalid buffer release"))?
                        .buffer_id;
                    self.release(id)?;
                }
                kind => {
                    // A synchronous allocation response can be interleaved with
                    // any valid asynchronous event during rapid resize. Routing
                    // those events here preserves the one protocol state machine;
                    // dropping them would lose input or presentation progress.
                    match parse_event(kind, frame.payload(), self.surface_id)
                        .ok_or_else(|| invalid("buffer response missing"))?
                    {
                        WireEvent::Public(event) => self.pending.push_back(event),
                        WireEvent::Released(id) => self.release(id)?,
                        progress @ (WireEvent::Accepted(_) | WireEvent::Presented(_)) => {
                            self.handle_progress(progress)?;
                        }
                    }
                }
            }
        };
        for descriptor in allocated.buffers.iter().take(count as usize) {
            self.buffers.push(Buffer {
                id: descriptor.buffer_id,
                pixels: self.device.map_shared_dumb(
                    descriptor.gem_handle,
                    physical.width as usize,
                    physical.height as usize,
                    descriptor.pitch as usize,
                    descriptor.byte_len as usize,
                )?,
                free: true,
            });
        }
        Ok(())
    }
}
