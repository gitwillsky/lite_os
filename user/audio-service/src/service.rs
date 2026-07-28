use std::{
    fs,
    io::{self, Read},
    os::{
        fd::AsFd,
        unix::fs::{MetadataExt, PermissionsExt},
        unix::net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use audio_proto::{
    ClientFrameReceiver, ClientReceive, ClientRole, ErrorCode, MAX_SESSION_STREAMS, SOCKET_PATH,
    ServiceMessage, send_service,
};
use linux_uapi::unix::{PollEvents, PollFd, peer_credentials, poll};

use crate::{
    mixer::{COMMAND_CAPACITY, EVENT_CAPACITY, Mixer, MixerCommand, MixerEvent, PlaybackDevice},
    queue::SpscQueue,
    settings::Settings,
};

mod client;
mod events;
#[cfg(test)]
mod tests;

const SETTINGS_PATH: &str = "/var/lib/liteos/audio/master";
const PCM_PATH: &str = "/dev/snd/pcmC0D0p";
const MAX_CONNECTIONS: usize = 64;

/// Runs the system audio service.
///
/// `diagnostic_log` enables periodic progress and mixer metric records. Errors,
/// lifecycle changes and master-state changes remain visible in either mode.
pub(crate) fn run(diagnostic_log: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !Path::new(PCM_PATH).exists() {
        eprintln!("audio-service: unavailable pcm={PCM_PATH}");
        // Secondary platforms without a sound device keep the supervised
        // process alive without probing or waking. If init respawned ENODEV,
        // it would create an unbounded log/CPU storm.
        loop {
            thread::park();
        }
    }
    let device = crate::alsa::AlsaDevice::open()?;
    run_with_device(device, PathBuf::from(SETTINGS_PATH), diagnostic_log)
}

pub(crate) fn run_with_device<D: PlaybackDevice>(
    device: D,
    settings_path: PathBuf,
    diagnostic_log: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut service = Service::start(device, settings_path, diagnostic_log)?;
    eprintln!("audio-service: ready");
    service.run_loop()
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct Connection {
    identity: u64,
    pid: i32,
    stream: UnixStream,
    receiver: ClientFrameReceiver,
    role: Option<ClientRole>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum StreamPhase {
    Live,
    Flushing(u64),
    Closing,
}

struct StreamRecord {
    connection: u64,
    pid: i32,
    id: u64,
    generation: u64,
    phase: StreamPhase,
    confirmed_frames: u64,
    next_progress_marker: u64,
}

impl StreamRecord {
    fn mixer_generation(&self) -> u64 {
        match self.phase {
            StreamPhase::Flushing(next) => next,
            StreamPhase::Live | StreamPhase::Closing => self.generation,
        }
    }
}

struct Service<D: PlaybackDevice> {
    listener: UnixListener,
    _socket_guard: SocketGuard,
    event_wake: UnixStream,
    connections: Vec<Connection>,
    streams: Vec<StreamRecord>,
    next_connection: u64,
    next_stream: u64,
    settings: Settings,
    commands: Arc<SpscQueue<MixerCommand, COMMAND_CAPACITY>>,
    events: Arc<SpscQueue<MixerEvent, EVENT_CAPACITY>>,
    mixer_failed: Arc<AtomicBool>,
    mixer: Option<JoinHandle<()>>,
    diagnostic_log: bool,
    _device: std::marker::PhantomData<D>,
}

impl<D: PlaybackDevice> Service<D> {
    fn start(device: D, settings_path: PathBuf, diagnostic_log: bool) -> io::Result<Self> {
        let (listener, socket_guard) = bind_listener(Path::new(SOCKET_PATH))?;
        listener.set_nonblocking(true)?;
        let settings = Settings::load(settings_path)?;
        let commands = Arc::new(SpscQueue::new());
        let events = Arc::new(SpscQueue::new());
        let mixer_failed = Arc::new(AtomicBool::new(false));
        let (event_wake, mixer_wake) = UnixStream::pair()?;
        event_wake.set_nonblocking(true)?;
        mixer_wake.set_nonblocking(true)?;
        let mixer = Mixer::new(
            device,
            commands.clone(),
            events.clone(),
            mixer_wake,
            mixer_failed.clone(),
            settings.state().gain(),
        );
        let handle = thread::Builder::new()
            .name("audio-mixer".to_owned())
            .spawn(move || mixer.run())?;
        Ok(Self {
            listener,
            _socket_guard: socket_guard,
            event_wake,
            connections: Vec::new(),
            streams: Vec::with_capacity(MAX_SESSION_STREAMS),
            next_connection: 1,
            next_stream: 1,
            settings,
            commands,
            events,
            mixer_failed,
            mixer: Some(handle),
            diagnostic_log,
            _device: std::marker::PhantomData,
        })
    }

    fn run_loop(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            if self.mixer_failed.load(Ordering::Acquire) {
                return Err("mixer queue or wake invariant failed".into());
            }
            let timeout = self.settings.timeout(Instant::now());
            let (listener_ready, events_ready, connection_ready) = {
                let mut descriptors = Vec::with_capacity(self.connections.len() + 2);
                descriptors.push(PollFd::new(self.listener.as_fd(), PollEvents::READ));
                descriptors.push(PollFd::new(self.event_wake.as_fd(), PollEvents::READ));
                for connection in &self.connections {
                    descriptors.push(PollFd::new(
                        connection.stream.as_fd(),
                        PollEvents::READ | PollEvents::ERROR | PollEvents::HANGUP,
                    ));
                }
                loop {
                    match poll(&mut descriptors, timeout) {
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        result => {
                            result?;
                            break;
                        }
                    }
                }
                let listener_ready = descriptors[0].returned().contains(PollEvents::READ);
                let events_ready = descriptors[1].returned().contains(PollEvents::READ);
                let connection_ready = descriptors[2..]
                    .iter()
                    .map(|descriptor| {
                        let returned = descriptor.returned();
                        returned.contains(PollEvents::READ)
                            || returned.contains(PollEvents::ERROR)
                            || returned.contains(PollEvents::HANGUP)
                    })
                    .collect::<Vec<_>>();
                (listener_ready, events_ready, connection_ready)
            };
            if events_ready {
                self.drain_event_wake()?;
                if self.drain_mixer_events()? {
                    return Err("physical audio device failed".into());
                }
            }
            if listener_ready {
                self.accept_connections()?;
            }
            let mut disconnected = Vec::new();
            for index in (0..connection_ready.len()).rev() {
                if connection_ready[index] && !self.receive_connection(index)? {
                    disconnected.push(self.connections[index].identity);
                    self.connections.swap_remove(index);
                }
            }
            for identity in disconnected {
                self.close_connection_streams(identity)?;
            }
            self.settings.flush_if_due(Instant::now())?;
        }
    }

    fn accept_connections(&mut self) -> io::Result<()> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.connections.len() >= MAX_CONNECTIONS {
                        drop(stream);
                        continue;
                    }
                    let credentials = peer_credentials(stream.as_fd())?;
                    if self
                        .connections
                        .iter()
                        .any(|connection| connection.pid == credentials.pid)
                    {
                        drop(stream);
                        continue;
                    }
                    stream.set_nonblocking(true)?;
                    let identity = self.next_connection;
                    self.next_connection = self
                        .next_connection
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("audio connection identity exhausted"))?;
                    self.connections.push(Connection {
                        identity,
                        pid: credentials.pid,
                        stream,
                        receiver: ClientFrameReceiver::new(),
                        role: None,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn receive_connection(&mut self, index: usize) -> io::Result<bool> {
        loop {
            let received = {
                let connection = &mut self.connections[index];
                connection.receiver.receive(&connection.stream)
            };
            match received {
                Ok(ClientReceive::Message(message)) => {
                    if !self.handle_message(index, message)? {
                        return Ok(false);
                    }
                }
                Ok(ClientReceive::Pending) => return Ok(true),
                Ok(ClientReceive::Closed) => return Ok(false),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) fn push_command(&self, command: MixerCommand) -> io::Result<()> {
        let edge = self.commands.push(command).map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "audio mixer command backpressure",
            )
        })?;
        if edge && let Some(handle) = &self.mixer {
            handle.thread().unpark();
        }
        Ok(())
    }

    fn drain_event_wake(&mut self) -> io::Result<()> {
        let mut bytes = [0; 64];
        loop {
            match self.event_wake.read(&mut bytes) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "mixer wake closed",
                    ));
                }
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) fn send_error(
        &self,
        index: usize,
        stream_id: Option<u64>,
        generation: u64,
        code: ErrorCode,
    ) -> io::Result<bool> {
        self.send(
            index,
            ServiceMessage::Error {
                stream_id,
                generation,
                code,
            },
        )
    }

    pub(super) fn send(&self, index: usize, message: ServiceMessage) -> io::Result<bool> {
        match send_service(&self.connections[index].stream, message, None) {
            Ok(()) => Ok(true),
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn record(
        &self,
        connection: u64,
        id: u64,
        generation: u64,
    ) -> Option<&StreamRecord> {
        self.streams.iter().find(|record| {
            record.connection == connection && record.id == id && record.generation == generation
        })
    }

    pub(super) fn record_mut(
        &mut self,
        connection: u64,
        id: u64,
        generation: u64,
    ) -> Option<&mut StreamRecord> {
        self.streams.iter_mut().find(|record| {
            record.connection == connection && record.id == id && record.generation == generation
        })
    }

    pub(super) fn connection_index(&self, identity: u64) -> Option<usize> {
        self.connections
            .iter()
            .position(|connection| connection.identity == identity)
    }
}

fn bind_listener(path: &Path) -> io::Result<(UnixListener, SocketGuard)> {
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => match UnixStream::connect(path) {
            Ok(active) => {
                drop(active);
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "audio service is already active",
                ));
            }
            Err(connect_error)
                if matches!(
                    connect_error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                fs::remove_file(path)?;
                UnixListener::bind(path)?
            }
            Err(connect_error) => return Err(connect_error),
        },
        Err(error) => return Err(error),
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let metadata = fs::metadata(path)?;
    Ok((
        listener,
        SocketGuard {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    ))
}

impl<D: PlaybackDevice> Drop for Service<D> {
    fn drop(&mut self) {
        let _ = self.settings.flush();
        let _ = self.push_command(MixerCommand::Shutdown);
        if let Some(handle) = self.mixer.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}
