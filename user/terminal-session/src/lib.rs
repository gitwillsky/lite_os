//! Pure PTY/VT helper for the React terminal application.

mod model;

use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsFd, BorrowedFd},
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
const SELECT: u32 = 6;
const SCROLL: u32 = 7;
const APPLICATION_CURSOR_KEYS: u32 = 1;
const MAX_INPUT: usize = 64 * 1024;
// 每轮最多消费 64 KiB PTY 输出，随后必须回到 control fd。缺少该预算时，Claude 等持续
// 刷新的 TUI 会让 drain 永远等不到 EAGAIN，LiteUI 的 ACK 与键盘输入因此永久饥饿。
const MAX_PTY_DRAIN_BYTES: usize = 64 * 1024;
const LINUX_EIO: i32 = 5;
const SHELL_PROMPT: &str = "\x1b[36m>\x1b[0m ";

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
    let environment = [(OsString::from("PS1"), OsString::from(SHELL_PROMPT))];
    let mut session = PtySession::spawn(size, &program, &arguments, &environment)?;
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
    let mut input = duplicate_control_input(io::stdin().as_fd())?;
    let mut output = io::stdout().lock();
    let mut update = Vec::new();
    send_update(&mut output, &mut update, &mut model)?;
    let mut in_flight = true;
    // OWNER: published mode tracks the last UPDATE acknowledged by LiteUI. Without this
    // projection, a mode-only DECSET transition leaves navigation keys encoded for the old
    // mode even though the VT parser has already accepted the new mode.
    let mut published_application_cursor_keys = model.application_cursor_keys();

    loop {
        let (control_ready, pty_ready) = {
            let mut descriptors = [
                PollFd::new(input.as_fd(), PollEvents::READ),
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
                Control::Input(bytes) => {
                    model.scroll_viewport(i32::MIN);
                    model.clear_selection();
                    write_pty(&mut session, &bytes)?;
                }
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
                Control::Select {
                    anchor_column,
                    anchor_row,
                    focus_column,
                    focus_row,
                } => {
                    model.set_selection(
                        usize::from(anchor_column),
                        usize::from(anchor_row),
                        usize::from(focus_column),
                        usize::from(focus_row),
                    );
                }
                Control::Scroll(lines) => {
                    model.scroll_viewport(lines);
                }
                Control::Eof => return Ok(()),
            }
        }
        if pty_ready && read_pty(&mut session, &mut model)? {
            send_exit(&mut output)?;
            return Ok(());
        }
        let application_cursor_keys = model.application_cursor_keys();
        if !in_flight
            && ((0..model.rows()).any(|row| model.dirty_span(row).is_some())
                || application_cursor_keys != published_application_cursor_keys
                || model.selection_dirty())
        {
            send_update(&mut output, &mut update, &mut model)?;
            published_application_cursor_keys = application_cursor_keys;
            in_flight = true;
        }
    }
}

fn duplicate_control_input(fd: BorrowedFd<'_>) -> io::Result<File> {
    // `poll` 与 `read_control` 必须观察同一个 kernel queue。`StdinLock` 会预读后续 frame；
    // fd 随即变成 not-ready，主循环会永久漏掉已经藏在 userspace buffer 里的键盘输入。
    fd.try_clone_to_owned().map(File::from)
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
    Select {
        anchor_column: u16,
        anchor_row: u16,
        focus_column: u16,
        focus_row: u16,
    },
    Scroll(i32),
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
        SELECT if payload.len() == 8 => Ok(Control::Select {
            anchor_column: u16::from_le_bytes(payload[0..2].try_into().expect("anchor column")),
            anchor_row: u16::from_le_bytes(payload[2..4].try_into().expect("anchor row")),
            focus_column: u16::from_le_bytes(payload[4..6].try_into().expect("focus column")),
            focus_row: u16::from_le_bytes(payload[6..8].try_into().expect("focus row")),
        }),
        SCROLL if payload.len() == 4 => Ok(Control::Scroll(i32::from_le_bytes(
            payload.try_into().expect("scroll lines"),
        ))),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal control kind invalid",
        )),
    }
}

fn read_pty(session: &mut PtySession, model: &mut Model) -> io::Result<bool> {
    let mut bytes = [0u8; 8192];
    let mut drained = 0usize;
    loop {
        match session.read(&mut bytes) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                let mut replies = Vec::new();
                model.feed(&bytes[..count], |reply| replies.extend_from_slice(reply));
                if !replies.is_empty() {
                    write_pty(session, &replies)?;
                }
                drained += count;
                if drained >= MAX_PTY_DRAIN_BYTES {
                    return Ok(false);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            // Linux PTY master 在最后一个 slave 关闭且输出已排空后返回 EIO；这与 read(2)
            // 返回 0 一样表示 session child 已退出，不是 helper failure。
            Err(error) if error.raw_os_error() == Some(LINUX_EIO) => return Ok(true),
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
    let active_dirty_rows = (0..model.rows())
        .filter(|row| model.dirty_span(*row).is_some())
        .count();
    let full_viewport = model.viewport_offset() != 0 && active_dirty_rows != 0;
    let dirty_rows = if full_viewport {
        model.rows()
    } else {
        active_dirty_rows
    };
    let selection = model.selection_projection();
    let selection_text_len = selection
        .as_ref()
        .map_or(0, |selection| selection.text.len());
    let payload = 40usize
        .checked_add(
            dirty_rows
                .checked_mul(4 + model.columns() * 16)
                .ok_or_else(|| io::Error::other("terminal update size overflow"))?,
        )
        .and_then(|payload| payload.checked_add(selection_text_len))
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
    bytes.extend_from_slice(&model.cursor_style().to_le_bytes());
    // The header ends with terminal default palette colors so the reader can
    // fill the unoccupied viewport and cursor without leaking the parser's
    // transient SGR rendition across an arbitrary UPDATE boundary.
    let (foreground, background) = model.default_colors();
    bytes.extend_from_slice(&foreground.to_le_bytes());
    bytes.extend_from_slice(&background.to_le_bytes());
    let input_modes = if model.application_cursor_keys() {
        APPLICATION_CURSOR_KEYS
    } else {
        0
    };
    bytes.extend_from_slice(&input_modes.to_le_bytes());
    if let Some(selection) = &selection {
        bytes.extend_from_slice(&(selection.start.column as u16).to_le_bytes());
        bytes.extend_from_slice(&(selection.start.row as u16).to_le_bytes());
        bytes.extend_from_slice(&(selection.end.column as u16).to_le_bytes());
        bytes.extend_from_slice(&(selection.end.row as u16).to_le_bytes());
    } else {
        bytes.extend_from_slice(&[0xff; 8]);
    }
    bytes.extend_from_slice(
        &u32::try_from(selection_text_len)
            .map_err(|_| io::Error::other("terminal selection text overflow"))?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&(model.viewport_offset() as u16).to_le_bytes());
    bytes.extend_from_slice(&(model.history_rows() as u16).to_le_bytes());
    for row in 0..model.rows() {
        if !full_viewport && model.dirty_span(row).is_none() {
            continue;
        }
        bytes.extend_from_slice(&(row as u16).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        for column in 0..model.columns() {
            bytes.extend_from_slice(&model.projected_cell(row, column).encode());
        }
        model.clear_dirty(row);
    }
    if let Some(selection) = selection {
        bytes.extend_from_slice(selection.text.as_bytes());
    }
    model.clear_selection_dirty();
    output.write_all(bytes)?;
    output.flush()
}

fn send_exit(output: &mut impl Write) -> io::Result<()> {
    output.write_all(&8u32.to_le_bytes())?;
    output.write_all(&EXIT.to_le_bytes())?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        os::{fd::AsFd, unix::net::UnixStream},
        time::Duration,
    };

    use linux_uapi::unix::{self, PollEvents, PollFd};

    use super::{
        ACK, APPLICATION_CURSOR_KEYS, Control, Grid, INPUT, Model, duplicate_control_input,
        read_control, send_update,
    };

    #[test]
    fn adjacent_control_frames_remain_visible_to_poll() {
        let (receiver, mut sender) = UnixStream::pair().unwrap();
        let mut frames = Vec::new();
        frames.extend_from_slice(&8u32.to_le_bytes());
        frames.extend_from_slice(&ACK.to_le_bytes());
        frames.extend_from_slice(&9u32.to_le_bytes());
        frames.extend_from_slice(&INPUT.to_le_bytes());
        frames.push(b'x');
        sender.write_all(&frames).unwrap();

        let mut input = duplicate_control_input(receiver.as_fd()).unwrap();
        assert!(matches!(read_control(&mut input).unwrap(), Control::Ack));

        let mut descriptors = [PollFd::new(input.as_fd(), PollEvents::READ)];
        assert_eq!(
            unix::poll(&mut descriptors, Some(Duration::ZERO)).unwrap(),
            1
        );
        assert!(descriptors[0].returned().contains(PollEvents::READ));
        assert!(matches!(
            read_control(&mut input).unwrap(),
            Control::Input(bytes) if bytes == b"x"
        ));
    }

    #[test]
    fn dec_application_cursor_mode_is_projected_by_the_vt_owner() {
        let mut model = Model::new(80, 25).unwrap();
        assert!(!model.application_cursor_keys());
        model.feed(b"\x1b[?1h", |_| {});
        assert!(model.application_cursor_keys());
        assert_eq!(APPLICATION_CURSOR_KEYS, 1);
        model.feed(b"\x1b[?1l", |_| {});
        assert!(!model.application_cursor_keys());
    }

    #[test]
    fn transient_sgr_colors_do_not_recolor_the_unoccupied_viewport() {
        let mut model = Model::new(4, 2).unwrap();
        let default_colors = model.default_colors();
        model.feed(b"\x1b[31;46mX", |_| {});
        assert_ne!(model.cell(0, 0).foreground, default_colors.0);
        assert_ne!(model.cell(0, 0).background, default_colors.1);

        let mut output = Vec::new();
        let mut frame = Vec::new();
        send_update(&mut output, &mut frame, &mut model).unwrap();
        let published_foreground =
            u32::from_le_bytes(output[20..24].try_into().expect("foreground"));
        let published_background =
            u32::from_le_bytes(output[24..28].try_into().expect("background"));
        assert_eq!((published_foreground, published_background), default_colors);
    }
}
