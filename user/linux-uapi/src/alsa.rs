//! Typed Linux ALSA PCM playback for LiteOS's fixed output contract.

use std::{
    ffi::{CString, c_void},
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    ptr,
};

use crate::{
    raw,
    shared_memory::SharedMapping,
    unix::{PollEvents, PollFd, poll},
};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const PERIOD_FRAMES: usize = 256;
pub const BUFFER_FRAMES: usize = 1024;
const FRAME_BYTES: usize = CHANNELS * size_of::<f32>();
const BUFFER_BYTES: usize = BUFFER_FRAMES * FRAME_BYTES;
const BOUNDARY: u64 = 1 << 31;

const IOCTL_PVERSION: usize = 0x8004_4100;
const IOCTL_HW_PARAMS: usize = 0xc260_4111;
const IOCTL_HW_FREE: usize = 0x0000_4112;
const IOCTL_SW_PARAMS: usize = 0xc088_4113;
const IOCTL_STATUS: usize = 0x8098_4120;
const IOCTL_DELAY: usize = 0x8008_4121;
const IOCTL_SYNC_PTR: usize = 0xc088_4123;
const IOCTL_PREPARE: usize = 0x0000_4140;
const IOCTL_START: usize = 0x0000_4142;
const IOCTL_DROP: usize = 0x0000_4143;

const HW_PARAMS_BYTES: usize = 608;
const SW_PARAMS_BYTES: usize = 136;
const STATUS_BYTES: usize = 152;
const SYNC_PTR_BYTES: usize = 136;

/// Linux `snd_pcm_state_t` values returned by [`PlaybackPcm::state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum State {
    Open = 0,
    Setup = 1,
    Prepared = 2,
    Running = 3,
    Xrun = 4,
    Disconnected = 8,
}

/// The fixed playback PCM and its owned data mapping.
pub struct PlaybackPcm {
    fd: OwnedFd,
    mapping: SharedMapping,
    application_frames: u64,
    prepared: bool,
    active: bool,
}

impl PlaybackPcm {
    /// Opens and configures `/dev/snd/pcmC0D0p` and maps its data ring.
    pub fn open() -> io::Result<Self> {
        let path = CString::new("/dev/snd/pcmC0D0p").expect("static PCM path");
        let raw_fd = unsafe {
            raw::open(
                path.as_ptr(),
                raw::O_RDWR | raw::O_NONBLOCK | raw::O_CLOEXEC,
                0,
            )
        };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        verify_protocol(fd.as_fd())?;
        configure_hardware(fd.as_fd())?;
        configure_software(fd.as_fd())?;
        let mapping = SharedMapping::map(fd.as_fd(), BUFFER_BYTES)?;
        Ok(Self {
            fd,
            mapping,
            application_frames: 0,
            prepared: false,
            active: false,
        })
    }

    /// Starts a prepared playback stream.
    pub fn start(&mut self) -> io::Result<()> {
        ioctl_unit(self.fd.as_fd(), IOCTL_START)?;
        self.active = true;
        Ok(())
    }

    /// Prepares a setup stream for a new four-period prefill cycle.
    pub fn prepare(&mut self) -> io::Result<()> {
        ioctl_unit(self.fd.as_fd(), IOCTL_PREPARE)?;
        self.application_frames = 0;
        self.prepared = true;
        self.active = false;
        Ok(())
    }

    /// Waits for one period slot or returns the PCM error state.
    pub fn wait_period(&self) -> io::Result<()> {
        let mut descriptor = [PollFd::new(
            self.fd.as_fd(),
            PollEvents::WRITE | PollEvents::ERROR | PollEvents::HANGUP,
        )];
        poll(&mut descriptor, None)?;
        let returned = descriptor[0].returned();
        if returned.contains(PollEvents::ERROR) || returned.contains(PollEvents::HANGUP) {
            return Err(io::Error::from_raw_os_error(
                if self.state()? == State::Xrun { 32 } else { 5 },
            ));
        }
        Ok(())
    }

    /// Copies and commits exactly one 256-frame stereo float period.
    pub fn write_period(&mut self, frames: &[[f32; CHANNELS]; PERIOD_FRAMES]) -> io::Result<()> {
        let ring_frame = self.application_frames as usize % BUFFER_FRAMES;
        let byte_offset = ring_frame * FRAME_BYTES;
        // SAFETY: ring_frame is period-aligned, so a fixed period remains within the 1024-frame
        // mapping. The unique &mut self serializes producer writes and sync-pointer publication.
        unsafe {
            ptr::copy_nonoverlapping(
                frames.as_ptr().cast::<u8>(),
                self.mapping.as_non_null().as_ptr().add(byte_offset),
                PERIOD_FRAMES * FRAME_BYTES,
            )
        };
        let next = (self.application_frames + PERIOD_FRAMES as u64) % BOUNDARY;
        let mut sync = [0u8; SYNC_PTR_BYTES];
        write_u64(&mut sync, 72, next);
        ioctl_bytes(self.fd.as_fd(), IOCTL_SYNC_PTR, &mut sync)?;
        self.application_frames = next;
        Ok(())
    }

    /// Returns the current queued device delay in frames.
    pub fn delay_frames(&self) -> io::Result<u32> {
        let mut delay = 0i64;
        ioctl_pointer(
            self.fd.as_fd(),
            IOCTL_DELAY,
            (&raw mut delay).cast::<c_void>(),
        )?;
        u32::try_from(delay)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative ALSA playback delay"))
    }

    /// Returns the current kernel PCM state.
    pub fn state(&self) -> io::Result<State> {
        let mut status = [0u8; STATUS_BYTES];
        ioctl_bytes(self.fd.as_fd(), IOCTL_STATUS, &mut status)?;
        match read_u32(&status, 0) as i32 {
            0 => Ok(State::Open),
            1 => Ok(State::Setup),
            2 => Ok(State::Prepared),
            3 => Ok(State::Running),
            4 => Ok(State::Xrun),
            8 => Ok(State::Disconnected),
            value => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported PCM state {value}"),
            )),
        }
    }

    /// Recovers an XRUN to the prepared state; the caller explicitly starts again.
    pub fn recover_xrun(&mut self) -> io::Result<()> {
        self.prepare()
    }

    /// Stops and releases the current hardware setup.
    pub fn stop(&mut self) {
        if self.prepared {
            let _ = ioctl_unit(self.fd.as_fd(), IOCTL_DROP);
            self.prepared = false;
            self.active = false;
        }
    }
}

impl AsFd for PlaybackPcm {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Drop for PlaybackPcm {
    fn drop(&mut self) {
        self.stop();
        let _ = ioctl_unit(self.fd.as_fd(), IOCTL_HW_FREE);
    }
}

fn verify_protocol(fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut version = 0i32;
    ioctl_pointer(fd, IOCTL_PVERSION, (&raw mut version).cast())?;
    if version >> 16 != 2 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "incompatible ALSA PCM protocol",
        ));
    }
    Ok(())
}

fn configure_hardware(fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut bytes = [0u8; HW_PARAMS_BYTES];
    set_mask(&mut bytes, 4, 0);
    set_mask(&mut bytes, 36, 14);
    set_mask(&mut bytes, 68, 0);
    set_interval(&mut bytes, 8, 32);
    set_interval(&mut bytes, 9, 64);
    set_interval(&mut bytes, 10, CHANNELS as u32);
    set_interval(&mut bytes, 11, RATE);
    set_interval(&mut bytes, 13, PERIOD_FRAMES as u32);
    set_interval(&mut bytes, 14, (PERIOD_FRAMES * FRAME_BYTES) as u32);
    set_interval(&mut bytes, 15, 4);
    set_interval(&mut bytes, 17, BUFFER_FRAMES as u32);
    set_interval(&mut bytes, 18, BUFFER_BYTES as u32);
    ioctl_bytes(fd, IOCTL_HW_PARAMS, &mut bytes)
}

fn configure_software(fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut bytes = [0u8; SW_PARAMS_BYTES];
    write_u64(&mut bytes, 16, PERIOD_FRAMES as u64);
    write_u64(&mut bytes, 24, 1);
    write_u64(&mut bytes, 32, BUFFER_FRAMES as u64);
    write_u64(&mut bytes, 40, BUFFER_FRAMES as u64);
    write_u64(&mut bytes, 64, BOUNDARY);
    ioctl_bytes(fd, IOCTL_SW_PARAMS, &mut bytes)
}

fn set_mask(bytes: &mut [u8], offset: usize, bit: usize) {
    let word = bit / 32;
    write_u32(bytes, offset + word * 4, 1 << (bit % 32));
}

fn set_interval(bytes: &mut [u8], parameter: usize, value: u32) {
    let offset = 260 + (parameter - 8) * 12;
    write_u32(bytes, offset, value);
    write_u32(bytes, offset + 4, value);
    write_u32(bytes, offset + 8, 0b0100);
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(bytes[offset..offset + 4].try_into().expect("u32 layout"))
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
}

fn ioctl_unit(fd: BorrowedFd<'_>, request: usize) -> io::Result<()> {
    ioctl_pointer(fd, request, ptr::null_mut())
}

fn ioctl_bytes(fd: BorrowedFd<'_>, request: usize, bytes: &mut [u8]) -> io::Result<()> {
    ioctl_pointer(fd, request, bytes.as_mut_ptr().cast())
}

fn ioctl_pointer(fd: BorrowedFd<'_>, request: usize, argument: *mut c_void) -> io::Result<()> {
    if unsafe { raw::ioctl(fd.as_raw_fd(), request, argument) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_hardware_layout_matches_linux_uapi() {
        let mut bytes = [0u8; HW_PARAMS_BYTES];
        set_mask(&mut bytes, 4, 0);
        set_mask(&mut bytes, 36, 14);
        set_interval(&mut bytes, 10, 2);
        set_interval(&mut bytes, 11, 48_000);
        assert_eq!(read_u32(&bytes, 4), 1);
        assert_eq!(read_u32(&bytes, 36), 1 << 14);
        assert_eq!(read_u32(&bytes, 284), 2);
        assert_eq!(read_u32(&bytes, 296), 48_000);
        assert_eq!(HW_PARAMS_BYTES, 608);
        assert_eq!(SYNC_PTR_BYTES, 136);
    }
}
