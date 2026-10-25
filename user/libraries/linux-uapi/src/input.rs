//! Typed evdev device access.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use crate::raw;

#[derive(Clone, Copy)]
pub struct InputEvent(raw::InputEvent);

impl InputEvent {
    pub const EMPTY: Self = Self(raw::InputEvent {
        seconds: 0,
        microseconds: 0,
        kind: 0,
        code: 0,
        value: 0,
    });

    pub fn kind(self) -> u16 {
        self.0.kind
    }

    pub fn code(self) -> u16 {
        self.0.code
    }

    pub fn value(self) -> i32 {
        self.0.value
    }
}

#[derive(Clone, Copy)]
pub struct AbsoluteRange {
    pub minimum: i32,
    pub maximum: i32,
}

#[derive(Clone, Copy)]
pub enum AbsoluteAxis {
    X,
    Y,
}

pub struct InputDevice {
    file: File,
}

impl InputDevice {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(raw::O_NONBLOCK | raw::O_CLOEXEC)
            .open(path)?;
        Ok(Self { file })
    }

    pub fn name(&self) -> io::Result<String> {
        let mut name = [0u8; 128];
        if unsafe {
            raw::ioctl(
                self.file.as_raw_fd(),
                raw::EVIOCGNAME_128,
                name.as_mut_ptr().cast(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let length = name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len());
        Ok(String::from_utf8_lossy(&name[..length]).into_owned())
    }

    pub fn grab(&self) -> io::Result<()> {
        let mut enabled = 1i32;
        if unsafe {
            raw::ioctl(
                self.file.as_raw_fd(),
                raw::EVIOCGRAB,
                (&raw mut enabled).cast(),
            )
        } < 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn absolute_range(&self, axis: AbsoluteAxis) -> io::Result<AbsoluteRange> {
        let request = match axis {
            AbsoluteAxis::X => raw::EVIOCGABS_X,
            AbsoluteAxis::Y => raw::EVIOCGABS_Y,
        };
        let mut info = raw::InputAbsInfo::default();
        if unsafe { raw::ioctl(self.file.as_raw_fd(), request, (&raw mut info).cast()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(AbsoluteRange {
            minimum: info.minimum,
            maximum: info.maximum,
        })
    }

    pub fn read_events(&mut self, output: &mut [InputEvent]) -> io::Result<usize> {
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(
                output.as_mut_ptr().cast::<u8>(),
                std::mem::size_of_val(output),
            )
        };
        let count = self.file.read(bytes)?;
        if count % size_of::<InputEvent>() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "evdev returned a partial event",
            ));
        }
        Ok(count / size_of::<InputEvent>())
    }

    pub fn file(&self) -> &File {
        &self.file
    }
}
