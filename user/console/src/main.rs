//! 文本控制台：在显示器上渲染一个 PTY shell，经 seatd 与桌面 compositor 轮流持有显示器和键盘。
//!
//! 生命周期：
//! 1. 启动后向 seatd 注册为 `Role::Text`，此时不碰 DRM 与 evdev。
//! 2. 收到 `Acquire`：打开 DRM、键盘，必要时按屏幕尺寸创建/调整终端并启动 shell，全量重绘。
//! 3. 收到 `Release`：关闭 DRM 与键盘，回复 `Released`；shell 与屏幕内容继续存活（输出仍写入 model）。
//! 4. Ctrl+Alt+F2 向 seatd 请求切到桌面；该快捷键不会写入 PTY。

mod display;
mod frame;
mod seat;

use std::{
    ffi::OsString,
    io,
    os::fd::AsFd,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use keyboard::{Modifiers, terminal_bytes};
use linux_uapi::{
    input::{InputDevice, InputEvent},
    pty::{PtySession, WindowSize},
    unix::{self, PollEvents, PollFd},
};
use seat_proto::{ClientFrame, Role, ServerFrame};
use terminal_font::{CELL_HEIGHT, CELL_WIDTH, TerminalFont};
use terminal_model::{Grid, Model};

use display::Display;
use frame::{Cursor, Layout, cursor_of, render_row};
use seat::{Received, SeatLink};

const EV_KEY: u16 = 1;
const LINUX_EIO: i32 = 5;
/// 每轮最多消费的 PTY 输出；与 terminal-session 同样的防饥饿预算，保证键盘与 seat 帧及时处理。
const MAX_PTY_DRAIN_BYTES: usize = 64 * 1024;
/// shell 退出后重启的最短间隔；缺少它，坏掉的 /bin/sh 会让 console 空转重启。
const RESPAWN_INTERVAL: Duration = Duration::from_secs(1);
const SHELL_PROMPT: &str = "\x1b[36m>\x1b[0m ";

/// 已持有座位时才存在的资源；整体 drop 即交还显示器与键盘。
struct Held {
    display: Display,
    keyboards: Vec<InputDevice>,
    /// 下一次 present 必须重画全部行（刚 Acquire，屏幕内容未知）。
    full_redraw: bool,
    last_cursor: Option<Cursor>,
}

/// shell 及其终端模型；首次 Acquire 时按屏幕尺寸创建，之后与座位持有状态无关。
struct Terminal {
    model: Model,
    session: PtySession,
    spawned_at: Instant,
}

struct Console {
    font: TerminalFont,
    terminal: Option<Terminal>,
    held: Option<Held>,
    modifiers: Modifiers,
    announced_first_frame: bool,
    /// 已完成的 Acquire / Release 次数，只用于串口 marker，让运行时门禁区分第几次交接。
    acquisitions: u32,
    releases: u32,
}

/// 运行时门禁依赖串口 marker：多个进程同时写 UART 时，分段写入会把一行拆开。整行一次 write 保证
/// marker 子串完整。
fn mark(line: String) {
    use std::io::Write;
    let _ = std::io::stderr().write_all(format!("{line}\n").as_bytes());
}

fn main() {
    if let Err(error) = run() {
        eprintln!("console: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    // 没有显示设备时永不注册：seatd 看不到 Text session，也就不会把座位交给它。若改为退出，
    // init 的 respawn 会让它无限重启并刷屏。
    if !Display::available() {
        mark("console: no display device; idle".to_owned());
        loop {
            thread::park();
        }
    }
    let mut seat = SeatLink::new(Role::Text);
    let mut console = Console {
        font: TerminalFont::open()?,
        terminal: None,
        held: None,
        modifiers: Modifiers::default(),
        announced_first_frame: false,
        acquisitions: 0,
        releases: 0,
    };
    loop {
        seat.ensure_connected();
        let (seat_ready, pty_ready, ready_keyboards) = console.wait(&seat)?;
        if seat_ready {
            match seat.receive() {
                Received::Frame(frame) => console.seat_frame(frame, &mut seat)?,
                Received::Closed | Received::Idle => {}
            }
        }
        if pty_ready {
            console.drain_pty()?;
        }
        for index in ready_keyboards {
            console.keyboard(index, &mut seat)?;
        }
        console.respawn_if_exited()?;
        console.present()?;
    }
}

impl Console {
    /// 等待 seat、PTY 或键盘就绪；返回各自的就绪状态。
    fn wait(&self, seat: &SeatLink) -> io::Result<(bool, bool, Vec<usize>)> {
        let mut descriptors = Vec::new();
        let seat_index = seat.fd().map(|fd| {
            descriptors.push(PollFd::new(fd, PollEvents::READ));
            descriptors.len() - 1
        });
        let pty_index = self.terminal.as_ref().map(|terminal| {
            descriptors.push(PollFd::new(terminal.session.as_fd(), PollEvents::READ));
            descriptors.len() - 1
        });
        let keyboard_base = descriptors.len();
        if let Some(held) = &self.held {
            for device in &held.keyboards {
                descriptors.push(PollFd::new(device.file().as_fd(), PollEvents::READ));
            }
        }
        let timeout = seat
            .reconnect_in()
            .unwrap_or(Duration::from_secs(1))
            .min(Duration::from_secs(1));
        match unix::poll(&mut descriptors, Some(timeout)) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                return Ok((false, false, Vec::new()));
            }
            Err(error) => return Err(error),
        }
        let ready = |index: usize| descriptors[index].returned() != PollEvents::EMPTY;
        Ok((
            seat_index.is_some_and(ready),
            pty_index.is_some_and(ready),
            (keyboard_base..descriptors.len())
                .filter(|&index| ready(index))
                .map(|index| index - keyboard_base)
                .collect(),
        ))
    }

    fn seat_frame(&mut self, frame: ServerFrame, seat: &mut SeatLink) -> io::Result<()> {
        match frame {
            ServerFrame::Acquire => self.acquire(),
            ServerFrame::Release => {
                // 关闭 DRM 与键盘后才能回复；缺少先后顺序，compositor 拿 master 会 EBUSY。
                self.held = None;
                self.modifiers = Modifiers::default();
                self.releases += 1;
                mark(format!("console: released {}", self.releases));
                seat.send(ClientFrame::Released);
                Ok(())
            }
            ServerFrame::Switched(_) => Ok(()),
        }
    }

    fn acquire(&mut self) -> io::Result<()> {
        // 重复 Acquire（seatd 重启后重新注册）先交还旧资源，避免同一进程持有两个 master。
        self.held = None;
        let display = Display::open()?;
        let layout = display.layout();
        self.ensure_terminal(layout)?;
        self.held = Some(Held {
            display,
            keyboards: open_keyboards(),
            full_redraw: true,
            last_cursor: None,
        });
        self.acquisitions += 1;
        mark(format!("console: acquired {}", self.acquisitions));
        Ok(())
    }

    /// 首次 Acquire 时创建终端；之后屏幕尺寸变化则调整终端与 PTY 窗口大小。
    fn ensure_terminal(&mut self, layout: Layout) -> io::Result<()> {
        let size = window_size(layout);
        match self.terminal.as_mut() {
            None => {
                let model = new_model(layout)?;
                let session = spawn_shell(size)?;
                mark("console: shell spawned".to_owned());
                self.terminal = Some(Terminal {
                    model,
                    session,
                    spawned_at: Instant::now(),
                });
            }
            Some(terminal)
                if terminal.model.columns() != layout.columns
                    || terminal.model.rows() != layout.rows =>
            {
                let candidate = terminal
                    .model
                    .prepare_resize(layout.columns, layout.rows)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::OutOfMemory, "terminal resize failed")
                    })?;
                terminal.session.resize(size)?;
                terminal.model.commit_resize(candidate);
            }
            Some(_) => {}
        }
        Ok(())
    }

    fn drain_pty(&mut self) -> io::Result<()> {
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(());
        };
        let mut bytes = [0u8; 8192];
        let mut drained = 0usize;
        loop {
            match terminal.session.read(&mut bytes) {
                // EOF 与 EIO 都表示 shell 已退出；重启由 respawn_if_exited 处理。
                Ok(0) => return Ok(()),
                Ok(count) => {
                    let mut replies = Vec::new();
                    terminal
                        .model
                        .feed(&bytes[..count], |reply| replies.extend_from_slice(reply));
                    write_all(&mut terminal.session, &replies)?;
                    drained += count;
                    if drained >= MAX_PTY_DRAIN_BYTES {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.raw_os_error() == Some(LINUX_EIO) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    fn respawn_if_exited(&mut self) -> io::Result<()> {
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(());
        };
        let exited = terminal.session.has_exited()?;
        if !exited || terminal.spawned_at.elapsed() < RESPAWN_INTERVAL {
            return Ok(());
        }
        let size = WindowSize {
            columns: terminal.model.columns() as u16,
            rows: terminal.model.rows() as u16,
            pixel_width: (terminal.model.columns() * CELL_WIDTH) as u16,
            pixel_height: (terminal.model.rows() * CELL_HEIGHT) as u16,
        };
        terminal.session = spawn_shell(size)?;
        terminal.spawned_at = Instant::now();
        terminal.model.begin_shell_session();
        terminal.model.mark_all();
        mark("console: shell respawned".to_owned());
        Ok(())
    }

    fn keyboard(&mut self, index: usize, seat: &mut SeatLink) -> io::Result<()> {
        let Some(held) = self.held.as_mut() else {
            return Ok(());
        };
        let mut events = [InputEvent::EMPTY; 32];
        let mut keys = Vec::new();
        loop {
            match held.keyboards[index].read_events(&mut events) {
                Ok(0) => break,
                Ok(count) => keys.extend(
                    events[..count]
                        .iter()
                        .filter(|event| event.kind() == EV_KEY)
                        .map(|event| (u32::from(event.code()), event.value())),
                ),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        for (code, value) in keys {
            // Ctrl+Alt+Fn 在持有座位时由本进程识别（compositor 独占 grab 键盘，seatd 看不到）。
            let switch = value == 1 && self.modifiers.control && self.modifiers.alt;
            let target = switch
                .then(|| Role::from_function_key(code as u16))
                .flatten()
                .filter(|role| *role != Role::Text);
            if let Some(role) = target {
                seat.send(ClientFrame::Switch(role));
                continue;
            }
            let application_cursor_keys = self
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.model.application_cursor_keys());
            if let Some(bytes) =
                terminal_bytes(&mut self.modifiers, code, value, application_cursor_keys)
                && let Some(terminal) = self.terminal.as_mut()
            {
                terminal.model.scroll_viewport(i32::MIN);
                write_all(&mut terminal.session, &bytes)?;
            }
        }
        Ok(())
    }

    /// 持有座位时把脏行（以及光标移动涉及的旧/新行）画到屏幕并上传。
    fn present(&mut self) -> io::Result<()> {
        let (Some(held), Some(terminal)) = (self.held.as_mut(), self.terminal.as_mut()) else {
            return Ok(());
        };
        let layout = held.display.layout();
        let cursor = cursor_of(&terminal.model);
        let mut rows: Vec<usize> = if held.full_redraw {
            (0..layout.rows).collect()
        } else {
            (0..layout.rows)
                .filter(|&row| terminal.model.dirty_span(row).is_some())
                .collect()
        };
        if cursor != held.last_cursor {
            for row in [held.last_cursor, cursor]
                .into_iter()
                .flatten()
                .map(|c| c.row)
            {
                if row < layout.rows && !rows.contains(&row) {
                    rows.push(row);
                }
            }
        }
        if rows.is_empty() {
            return Ok(());
        }
        rows.sort_unstable();
        for &row in &rows {
            render_row(
                &terminal.model,
                &self.font,
                layout,
                held.display.buffer_mut(),
                row,
                cursor,
            );
            terminal.model.clear_dirty(row);
        }
        if held.full_redraw {
            held.display.flush(None)?;
        } else {
            // 连续行合并成一个 damage 带，避免每行一次 ioctl。
            let mut start = rows[0];
            let mut previous = rows[0];
            for &row in &rows[1..] {
                if row != previous + 1 {
                    held.display.flush(Some((start, previous)))?;
                    start = row;
                }
                previous = row;
            }
            held.display.flush(Some((start, previous)))?;
        }
        held.full_redraw = false;
        held.last_cursor = cursor;
        if !self.announced_first_frame {
            self.announced_first_frame = true;
            mark("console: first frame presented".to_owned());
        }
        Ok(())
    }
}

fn window_size(layout: Layout) -> WindowSize {
    WindowSize {
        columns: layout.columns as u16,
        rows: layout.rows as u16,
        pixel_width: (layout.columns * CELL_WIDTH) as u16,
        pixel_height: (layout.rows * CELL_HEIGHT) as u16,
    }
}

fn new_model(layout: Layout) -> io::Result<Model> {
    let mut model = Model::new(layout.columns, layout.rows).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::OutOfMemory,
            "terminal grid allocation failed",
        )
    })?;
    model.begin_shell_session();
    model.mark_all();
    Ok(model)
}

fn spawn_shell(size: WindowSize) -> io::Result<PtySession> {
    let environment = [(OsString::from("PS1"), OsString::from(SHELL_PROMPT))];
    PtySession::spawn(
        size,
        &OsString::from("/bin/sh"),
        &[OsString::from("-l")],
        &environment,
    )
}

fn write_all(session: &mut PtySession, mut bytes: &[u8]) -> io::Result<()> {
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
            // PTY 输入队列暂满：短暂让出后重试；shell 持续不读取时这里会自旋，但键盘输入速率远低于排空速率。
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// 打开所有名称含 "keyboard" 的 evdev 设备；不 grab，持有期间只有本进程读取（compositor 已关闭）。
fn open_keyboards() -> Vec<InputDevice> {
    (0..16u32)
        .filter_map(|index| {
            let device =
                InputDevice::open(&PathBuf::from(format!("/dev/input/event{index}"))).ok()?;
            device
                .name()
                .is_ok_and(|name| name.to_ascii_lowercase().contains("keyboard"))
                .then_some(device)
        })
        .collect()
}
