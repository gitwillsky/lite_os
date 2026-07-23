//! Pure PTY/VT helper for the React terminal application.

mod model;

use std::{
    ffi::OsString,
    io::{self, Read, Write},
    os::fd::AsFd,
    time::Duration,
};

use linux_uapi::{
    pty::{PtySession, WindowSize},
    unix::{self, PollEvents, PollFd},
};
use model::{Grid, Model};

const INPUT: u32 = 1;
const RESIZE: u32 = 2;
const ACK: u32 = 3;
const UPDATE: u32 = 4;
const EXIT: u32 = 5;
const MAX_INPUT: usize = 64 * 1024;

fn main() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("terminal-session: invariant failure: {info}")
    }));
    if let Err(error) = run() {
        eprintln!("terminal-session: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let (program, arguments) = command()?;
    let mut size = WindowSize {
        columns: 80,
        rows: 25,
        pixel_width: 640,
        pixel_height: 400,
    };
    let mut session = PtySession::spawn(size, &program, &arguments)?;
    eprintln!("terminal-session: shell spawned");
    let mut model =
        Model::new(usize::from(size.columns), usize::from(size.rows)).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "terminal grid allocation failed",
            )
        })?;
    model.begin_shell_session();
    model.mark_all();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stdout().lock();
    let mut update = Vec::new();
    send_update(&mut output, &mut update, &mut model)?;
    let mut in_flight = true;

    loop {
        let (control_ready, pty_ready) = {
            let mut descriptors = [
                PollFd::new(stdin.as_fd(), PollEvents::READ),
                PollFd::new(session.as_fd(), PollEvents::READ),
            ];
            unix::poll(&mut descriptors, Some(Duration::from_secs(1)))?;
            (
                descriptors[0].returned() != PollEvents::EMPTY,
                descriptors[1].returned() != PollEvents::EMPTY,
            )
        };
        if control_ready {
            match read_control(&mut input)? {
                Control::Input(bytes) => write_pty(&mut session, &bytes)?,
                Control::Resize(next) => {
                    size = next;
                    let candidate = model
                        .prepare_resize(usize::from(size.columns), usize::from(size.rows))
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::OutOfMemory,
                                "terminal resize allocation failed",
                            )
                        })?;
                    session.resize(size)?;
                    model.commit_resize(candidate);
                }
                Control::Ack => in_flight = false,
                Control::Eof => return Ok(()),
            }
        }
        if pty_ready && read_pty(&mut session, &mut model)? {
            send_exit(&mut output)?;
            return Ok(());
        }
        if !in_flight && (0..model.rows()).any(|row| model.dirty_span(row).is_some()) {
            send_update(&mut output, &mut update, &mut model)?;
            in_flight = true;
        }
    }
}

fn command() -> io::Result<(OsString, Vec<OsString>)> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: terminal-session -- <program> [arg ...]",
        ));
    }
    let program = arguments.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "terminal program is required")
    })?;
    Ok((program, arguments.collect()))
}

enum Control {
    Input(Vec<u8>),
    Resize(WindowSize),
    Ack,
    Eof,
}

fn read_control(input: &mut impl Read) -> io::Result<Control> {
    let mut header = [0u8; 8];
    match input.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(Control::Eof),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(header[..4].try_into().expect("control length")) as usize;
    let kind = u32::from_le_bytes(header[4..].try_into().expect("control kind"));
    if !(8..=MAX_INPUT + 8).contains(&length) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal control length invalid",
        ));
    }
    let mut payload = vec![0; length - 8];
    input.read_exact(&mut payload)?;
    match kind {
        INPUT => Ok(Control::Input(payload)),
        RESIZE if payload.len() == 8 => Ok(Control::Resize(WindowSize {
            columns: u16::from_le_bytes(payload[0..2].try_into().expect("columns")),
            rows: u16::from_le_bytes(payload[2..4].try_into().expect("rows")),
            pixel_width: u16::from_le_bytes(payload[4..6].try_into().expect("pixel width")),
            pixel_height: u16::from_le_bytes(payload[6..8].try_into().expect("pixel height")),
        })),
        ACK if payload.is_empty() => Ok(Control::Ack),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal control kind invalid",
        )),
    }
}

fn read_pty(session: &mut PtySession, model: &mut Model) -> io::Result<bool> {
    let mut bytes = [0u8; 8192];
    loop {
        match session.read(&mut bytes) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                let mut replies = Vec::new();
                model.feed(&bytes[..count], |reply| replies.extend_from_slice(reply));
                if !replies.is_empty() {
                    write_pty(session, &replies)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error),
        }
    }
}

fn write_pty(session: &mut PtySession, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match session.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "PTY write returned zero",
                ));
            }
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn send_update(output: &mut impl Write, bytes: &mut Vec<u8>, model: &mut Model) -> io::Result<()> {
    let dirty_rows = (0..model.rows())
        .filter(|row| model.dirty_span(*row).is_some())
        .count();
    let payload = 20usize
        .checked_add(
            dirty_rows
                .checked_mul(4 + model.columns() * 16)
                .ok_or_else(|| io::Error::other("terminal update size overflow"))?,
        )
        .ok_or_else(|| io::Error::other("terminal update size overflow"))?;
    bytes.clear();
    bytes.try_reserve(8 + payload).map_err(|_| {
        io::Error::new(
            io::ErrorKind::OutOfMemory,
            "terminal update allocation failed",
        )
    })?;
    bytes.extend_from_slice(&((8 + payload) as u32).to_le_bytes());
    bytes.extend_from_slice(&UPDATE.to_le_bytes());
    bytes.extend_from_slice(&(model.columns() as u16).to_le_bytes());
    bytes.extend_from_slice(&(model.rows() as u16).to_le_bytes());
    // Wire layout pins `columns, rows, cursor_column, cursor_row` in one order;
    // the lite-ui reader decodes the same sequence.
    let cursor = model
        .cursor()
        .unwrap_or((u16::MAX as usize, u16::MAX as usize));
    bytes.extend_from_slice(&(cursor.0 as u16).to_le_bytes());
    bytes.extend_from_slice(&(cursor.1 as u16).to_le_bytes());
    bytes.extend_from_slice(&(dirty_rows as u16).to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    // The header ends with the current default colors so the reader can fill
    // the container background and cursor without a per-cell trip.
    let (foreground, background) = model.default_colors();
    bytes.extend_from_slice(&foreground.to_le_bytes());
    bytes.extend_from_slice(&background.to_le_bytes());
    for row in 0..model.rows() {
        if model.dirty_span(row).is_none() {
            continue;
        }
        bytes.extend_from_slice(&(row as u16).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        for column in 0..model.columns() {
            bytes.extend_from_slice(&model.cell(row, column).encode());
        }
        model.clear_dirty(row);
    }
    output.write_all(bytes)?;
    output.flush()
}

fn send_exit(output: &mut impl Write) -> io::Result<()> {
    output.write_all(&8u32.to_le_bytes())?;
    output.write_all(&EXIT.to_le_bytes())?;
    output.flush()
}
