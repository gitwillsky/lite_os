//! UI-thread command and pollable event endpoints for the process audio worker.

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
};

use audio_proto::ClientRole;
use serde::Serialize;

use super::Worker;

pub(crate) enum Command {
    Load {
        id: u64,
        path: PathBuf,
        offset: u64,
        length: u64,
        /// Present when `path` is a still-downloading stream temp file; drives
        /// read back-pressure in the decoder. `None` for local files.
        stream: Option<super::SharedStream>,
    },
    Play {
        id: u64,
    },
    Pause {
        id: u64,
    },
    Seek {
        id: u64,
        seconds: f64,
    },
    Gain {
        id: u64,
        gain: f32,
    },
    Loop {
        id: u64,
        enabled: bool,
    },
    Close {
        id: u64,
    },
    GetMasterState,
    SetMasterVolume {
        percent: u8,
    },
    SetMasterMuted {
        muted: bool,
    },
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Event {
    pub(super) id: u64,
    #[serde(rename = "type")]
    pub(super) kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) duration: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<MediaError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) muted: Option<bool>,
}

#[derive(Clone, Serialize)]
pub(crate) struct MediaError {
    pub(super) code: u8,
    pub(super) message: String,
}

impl Event {
    pub(crate) fn channel(&self) -> &'static str {
        if self.kind == "masterstate" {
            "audio-system"
        } else {
            "media"
        }
    }

    pub(super) fn plain(id: u64, kind: &'static str) -> Self {
        Self {
            id,
            kind,
            duration: None,
            current_time: None,
            error: None,
            percent: None,
            muted: None,
        }
    }

    pub(super) fn time(id: u64, kind: &'static str, current_time: f64) -> Self {
        Self {
            current_time: Some(current_time),
            ..Self::plain(id, kind)
        }
    }

    pub(super) fn error(id: u64, code: u8, message: impl Into<String>) -> Self {
        Self {
            error: Some(MediaError {
                code,
                message: message.into(),
            }),
            ..Self::plain(id, "error")
        }
    }
}

/// UI-thread endpoint. Sending appends one command and edge-wakes the worker.
pub(crate) struct Commands {
    queue: Arc<Mutex<VecDeque<Command>>>,
    wake: UnixStream,
}

impl Commands {
    pub(crate) fn send(&mut self, command: Command) -> Result<(), String> {
        self.queue
            .lock()
            .map_err(|_| "audio command queue is poisoned".to_owned())?
            .push_back(command);
        self.wake
            .write_all(&[1])
            .map_err(|error| format!("audio worker wake failed: {error}"))
    }
}

/// Pollable read-side of the worker's bounded-owner event queue.
pub(crate) struct Events {
    queue: Arc<Mutex<VecDeque<Event>>>,
    wake: UnixStream,
}

impl Events {
    pub(crate) fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.wake.as_fd()
    }

    pub(crate) fn drain(&mut self) -> io::Result<Vec<Event>> {
        let mut bytes = [0; 256];
        loop {
            match self.wake.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => continue,
                // A drained nonblocking wake stream ends at WouldBlock; treating
                // that boundary as fatal would terminate the idle desktop.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(self
            .queue
            .lock()
            .map_err(|_| io::Error::other("audio event queue is poisoned"))?
            .drain(..)
            .collect())
    }
}

/// Starts exactly one process audio worker; idle workers block without a timer.
pub(crate) fn start(role: ClientRole) -> io::Result<(Commands, Events)> {
    let (command_read, command_write) = UnixStream::pair()?;
    let (event_read, event_write) = UnixStream::pair()?;
    event_read.set_nonblocking(true)?;
    let command_queue = Arc::new(Mutex::new(VecDeque::new()));
    let event_queue = Arc::new(Mutex::new(VecDeque::new()));
    let worker_commands = command_queue.clone();
    let worker_events = event_queue.clone();
    thread::Builder::new()
        .name("lite-ui-audio".to_owned())
        .spawn(move || {
            Worker::new(
                role,
                worker_commands,
                command_read,
                worker_events,
                event_write,
            )
            .run()
        })?;
    Ok((
        Commands {
            queue: command_queue,
            wake: command_write,
        },
        Events {
            queue: event_queue,
            wake: event_read,
        },
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex},
    };

    use super::Events;

    #[test]
    fn empty_nonblocking_event_drain_is_normal_idle() {
        let (wake, _notifier) = UnixStream::pair().expect("wake pair");
        wake.set_nonblocking(true).expect("nonblocking");
        let mut events = Events {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            wake,
        };
        assert!(events.drain().expect("idle drain").is_empty());
    }
}
