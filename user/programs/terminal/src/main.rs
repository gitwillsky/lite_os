//! 普通用户终端：共享 UI Canvas + Alacritty VT 引擎 + 标准 Linux PTY；无终端专用协议。
mod input;
mod model;
mod mouse;
mod view;
use input::Input;
use keyboard::Modifiers;
use linux_uapi::pty::{PtySession, WindowSize as PtySize};
use liteos_ui::{
    Application, ControlFlow, EventKind, Task, Widget, WidgetKind, WidgetSpec, WindowOptions,
};
use model::{Area, Model, TermMode};
use mouse::{Mouse, Position};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    future::{Future, poll_fn},
    io,
    pin::Pin,
    process::ExitCode,
    rc::Rc,
    task::Poll,
    time::{Duration, Instant},
};
type Session = Rc<RefCell<Option<PtySession>>>;
const TITLE: &str = "终端";

fn pty_size(area: Area) -> PtySize {
    let size = area.window_size();
    PtySize {
        columns: size.num_cols,
        rows: size.num_lines,
        pixel_width: size.num_cols * size.cell_width,
        pixel_height: size.num_lines * size.cell_height,
    }
}

fn resize(
    canvas: &Widget,
    model: &mut Model,
    session: &Session,
    cell: (i32, i32),
    scale: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    let size = view::geometry(canvas, cell, scale)?;
    if model.size() == size {
        return Ok(());
    }
    if let Some(session) = session.borrow().as_ref() {
        session.resize(pty_size(size))?;
    }
    model.resize(size);
    Ok(())
}

fn pointer_position(canvas: &Widget, size: Area, x: f64, y: f64) -> liteos_ui::Result<Position> {
    let rect = canvas.geometry()?;
    let (width, height) = size.cell;
    let column_px = (x as i32 - rect.x).clamp(0, i32::from(size.columns) * width - 1);
    let row_px = (y as i32 - rect.y).clamp(0, i32::from(size.rows) * height - 1);
    Ok(Position {
        row: (row_px / height) as usize,
        column: (column_px / width) as usize,
        right: column_px % width >= width / 2,
    })
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let program = args.next().unwrap_or_else(|| OsString::from("/bin/sh"));
    let arguments = args.collect::<Vec<_>>();
    let mut app = Application::new()?;
    let window = app.window(WindowOptions::new(TITLE, "org.liteos.terminal", 648, 420))?;
    let mut spec = WidgetSpec::new(WidgetKind::Canvas, "");
    spec.layout.grow = 1;
    spec.state = liteos_ui::state::TAB_INPUT;
    let canvas = window.widget(spec)?;
    // 窗口出现前完成初次 exec；此后所有等待、读写和终止回收通过异步任务推进。
    let initial = Area {
        columns: 80,
        rows: 24,
        cell: window.monospace_cell_size()?,
        scale: 1,
    };
    let environment = [
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
        (
            "LANG".into(),
            std::env::var_os("LANG").unwrap_or_else(|| "C.UTF-8".into()),
        ),
    ];
    let pty = PtySession::spawn(pty_size(initial), &program, &arguments, &environment)?;
    let read_fd = pty.as_fd().try_clone_to_owned()?;
    let write_fd = pty.as_fd().try_clone_to_owned()?;
    let session: Session = Rc::new(RefCell::new(Some(pty)));
    let model = Rc::new(RefCell::new(Model::new(initial)));
    let input = Input::new();
    let focused = Rc::new(Cell::new(false));
    let blink_on = Rc::new(Cell::new(true));
    let mut reader = app.watch(read_fd)?;
    let mut writer = app.watch(write_fd)?;
    let background = app.background()?;
    let reader_window = window.downgrade();
    let reader_session = session.clone();
    let reader_model = model.clone();
    let reader_input = input.clone();
    let reader_canvas = canvas.clone();
    let reader_focus = focused.clone();
    let reader_blink = blink_on.clone();
    let clock = app.clock();
    let _read = app.spawn_local(&window, async move {
        let publish = |update: model::Update| -> Result<(), Box<dyn std::error::Error>> {
            reader_input.push(update.reply)?;
            if let Some(title) = update.title
                && let Some(window) = reader_window.upgrade()
            {
                window.set_title(if title.is_empty() { TITLE } else { &title })?;
            }
            Ok(())
        };
        loop {
            let deadline = reader_model.borrow().sync_deadline();
            let mut ready = reader.readable();
            let wait = match deadline {
                Some(deadline) => {
                    let mut timer =
                        match clock.sleep(deadline.saturating_duration_since(Instant::now())) {
                            Ok(timer) => timer,
                            Err(error) => {
                                eprintln!("terminal: synchronized output: {error}");
                                break;
                            }
                        };
                    poll_fn(|cx| {
                        if Pin::new(&mut timer).poll(cx).is_ready() {
                            return Poll::Ready(Ok(false));
                        }
                        Pin::new(&mut ready)
                            .poll(cx)
                            .map(|result| result.map(|_| true))
                    })
                    .await
                }
                None => ready.await.map(|_| true),
            };
            match wait {
                Ok(true) => {}
                Ok(false) => {
                    let update = reader_model.borrow_mut().finish_sync();
                    if let Err(error) = publish(update).and_then(|()| {
                        Ok(view::paint(
                            &reader_canvas,
                            &reader_model.borrow(),
                            reader_focus.get(),
                            reader_blink.get(),
                        )?)
                    }) {
                        eprintln!("terminal: synchronized output: {error}");
                        break;
                    }
                    continue;
                }
                Err(error) => {
                    eprintln!("terminal: {error}");
                    break;
                }
            }
            let mut bytes = [0; 8192];
            let mut budget = 65536;
            let mut ended = false;
            loop {
                let result = {
                    let mut session = reader_session.borrow_mut();
                    let Some(session) = session.as_mut() else {
                        return;
                    };
                    let capacity = bytes.len().min(budget);
                    session.read(&mut bytes[..capacity])
                };
                match result {
                    Ok(0) => {
                        ended = true;
                        break;
                    }
                    Ok(count) => {
                        let update = reader_model.borrow_mut().feed(&bytes[..count]);
                        if let Err(error) = publish(update) {
                            eprintln!("terminal: PTY output: {error}");
                            ended = true;
                            break;
                        }
                        budget -= count;
                        if budget == 0 {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        budget -= 1;
                        if budget == 0 {
                            break;
                        }
                    }
                    // Linux PTY 在最后一个 slave 关闭时可返回 EIO，而不是零长度 EOF。
                    Err(error) => {
                        if error.raw_os_error() != Some(5) {
                            eprintln!("terminal: read: {error}");
                        }
                        ended = true;
                        break;
                    }
                }
            }
            if let Err(error) = view::paint(
                &reader_canvas,
                &reader_model.borrow(),
                reader_focus.get(),
                reader_blink.get(),
            ) {
                eprintln!("terminal: {error}");
                break;
            }
            if ended {
                break;
            }
        }
        let pty = reader_session.borrow_mut().take();
        match background.run(move || drop(pty)) {
            Ok(done) => done.await,
            Err(error) => eprintln!("terminal: cleanup: {error}"),
        }
        if let Some(window) = reader_window.upgrade() {
            let _ = window.close();
        }
    })?;
    let writer_session = session.clone();
    let writer_input = input.clone();
    let _write = app.spawn_local(&window, async move {
        loop {
            writer_input.pending().await;
            if let Err(error) = writer.writable().await {
                eprintln!("terminal: {error}");
                break;
            }
            let result = {
                let mut session = writer_session.borrow_mut();
                let Some(session) = session.as_mut() else {
                    return;
                };
                writer_input.write(session)
            };
            match result {
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => {
                    eprintln!("terminal: write: {error}");
                    break;
                }
            }
        }
    })?;
    let mut paste: Option<Task<()>> = None;
    let mut cleanup: Option<Task<()>> = None;
    let mut blinker: Option<Task<()>> = None;
    let mut pointer = (0., 0.);
    let mut selecting = false;
    let mut mouse = Mouse::default();
    let mut modifiers = Modifiers::default();
    app.run(|app, event| {
        if event.window != window.id() {
            return Ok(ControlFlow::Continue);
        }
        match event.kind {
            EventKind::Close => {
                if cleanup.is_none() {
                    let pty = session.borrow_mut().take();
                    let done = app.blocking(move || drop(pty))?;
                    let window = window.clone();
                    cleanup = Some(app.spawn(async move {
                        done.await;
                        let _ = window.close();
                    })?);
                }
                return Ok(ControlFlow::PreventDefault);
            }
            EventKind::Configure { scale, .. } => {
                if let Err(error) = resize(
                    &canvas,
                    &mut model.borrow_mut(),
                    &session,
                    window.monospace_cell_size()?,
                    scale,
                ) {
                    eprintln!("terminal: resize: {error}");
                }
                view::paint(&canvas, &model.borrow(), focused.get(), blink_on.get())?;
            }
            EventKind::Focus(value) => {
                focused.set(value);
                blink_on.set(true);
                selecting = false;
                mouse.cancel();
                if !value {
                    modifiers = Modifiers::default();
                }
                if model.borrow().mode().contains(TermMode::FOCUS_IN_OUT) {
                    input.send(if value { b"\x1b[I" } else { b"\x1b[O" }.to_vec());
                }
                // 焦点窗口才启动光标计时器；失焦即取消。
                blinker = if value {
                    let clock = app.clock();
                    let (canvas, model, blink_on) =
                        (canvas.clone(), model.clone(), blink_on.clone());
                    Some(app.spawn_local(&window, async move {
                        loop {
                            let Ok(timer) = clock.sleep(Duration::from_millis(500)) else {
                                break;
                            };
                            timer.await;
                            let model = model.borrow();
                            if model
                                .screen()
                                .cursor()
                                .is_some_and(|cursor| cursor.blinking)
                                && model.sync_deadline().is_none()
                            {
                                blink_on.set(!blink_on.get());
                                if let Err(error) =
                                    view::paint(&canvas, &model, true, blink_on.get())
                                {
                                    eprintln!("terminal: cursor: {error}");
                                    break;
                                }
                            }
                        }
                    })?)
                } else {
                    None
                };
                view::paint(&canvas, &model.borrow(), value, blink_on.get())?;
            }
            EventKind::Wheel {
                vertical,
                horizontal,
            } if event.node == canvas.id() => {
                let mut model = model.borrow_mut();
                let mode = model.mode();
                if mouse::reporting(mode, modifiers) {
                    let position = pointer_position(&canvas, model.size(), pointer.0, pointer.1)?;
                    input.send(Mouse::wheel(
                        mode, modifiers, position, vertical, horizontal,
                    ));
                } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
                    && !modifiers.shift
                {
                    let key = if vertical < 0 { 103 } else { 108 };
                    if let Some(bytes) =
                        keyboard::terminal_input(key, Modifiers::default(), None, true, false)
                    {
                        input.send(bytes.repeat(vertical.unsigned_abs().min(64) as usize));
                    }
                } else if model.scroll(vertical.saturating_neg().saturating_mul(3)) {
                    view::paint(&canvas, &model, focused.get(), blink_on.get())?;
                }
            }
            EventKind::Pointer { x, y } => {
                pointer = (x, y);
                let mut model = model.borrow_mut();
                let position = pointer_position(&canvas, model.size(), x, y)?;
                if selecting {
                    model.extend_selection(position.row, position.column, position.right);
                    view::paint(&canvas, &model, focused.get(), blink_on.get())?;
                } else if event.node == canvas.id()
                    && let Some(bytes) = mouse.motion(model.mode(), modifiers, position)
                {
                    input.send(bytes);
                }
            }
            EventKind::Button { code, pressed, .. } if (0x110..=0x112).contains(&code) => {
                if pressed && event.node != canvas.id() {
                    return Ok(ControlFlow::Continue);
                }
                let mut model = model.borrow_mut();
                let position = pointer_position(&canvas, model.size(), pointer.0, pointer.1)?;
                let button = match code {
                    0x110 => 0,
                    0x112 => 1,
                    _ => 2,
                };
                if let Some(bytes) =
                    mouse.button(model.mode(), modifiers, position, button, pressed)
                {
                    input.send(bytes);
                } else if button == 0 && (selecting || !mouse::reporting(model.mode(), modifiers)) {
                    selecting = pressed;
                    if pressed {
                        model.select(position.row, position.column, position.right);
                        view::paint(&canvas, &model, focused.get(), blink_on.get())?;
                    }
                }
            }
            EventKind::TextCommit(text) if event.node == canvas.id() => {
                if input.send(text.into_bytes()) && model.borrow_mut().follow_output() {
                    view::paint(&canvas, &model.borrow(), focused.get(), blink_on.get())?;
                }
            }
            EventKind::Key {
                code,
                pressed,
                modifiers: mask,
                text,
                handled,
                ..
            } => {
                modifiers = Modifiers {
                    shift: mask & 1 != 0,
                    control: mask & 2 != 0,
                    alt: mask & 4 != 0,
                    super_key: mask & 8 != 0,
                    caps: false,
                };
                if handled || !pressed {
                    return Ok(ControlFlow::Continue);
                }
                if mask == 3 && code == 46 {
                    if let Some(selection) = model.borrow().selected_text() {
                        app.set_clipboard(&selection)?;
                    }
                } else if mask == 3 && code == 47 {
                    let read = app.clipboard_text(Duration::from_secs(3))?;
                    let (target, model, canvas, focused, blink_on) = (
                        input.clone(),
                        model.clone(),
                        canvas.clone(),
                        focused.clone(),
                        blink_on.clone(),
                    );
                    paste = Some(app.spawn_local(&window, async move {
                        match read.await {
                            Ok(Some(text)) => {
                                let mut model = model.borrow_mut();
                                let bytes = input::paste(
                                    &text,
                                    model.mode().contains(TermMode::BRACKETED_PASTE),
                                );
                                if target.send(bytes)
                                    && model.follow_output()
                                    && let Err(error) =
                                        view::paint(&canvas, &model, focused.get(), blink_on.get())
                                {
                                    eprintln!("terminal: paste: {error}");
                                }
                            }
                            Ok(None) => {}
                            Err(error) => eprintln!("terminal: paste: {error}"),
                        }
                    })?);
                } else {
                    let mut model = model.borrow_mut();
                    let mode = model.mode();
                    if let Some(bytes) = keyboard::terminal_input(
                        code,
                        modifiers,
                        text,
                        mode.contains(TermMode::APP_CURSOR),
                        mode.contains(TermMode::APP_KEYPAD),
                    ) && input.send(bytes)
                        && model.follow_output()
                    {
                        view::paint(&canvas, &model, focused.get(), blink_on.get())?;
                    }
                }
            }
            _ => {}
        }
        Ok(ControlFlow::Continue)
    })?;
    drop(paste);
    drop(blinker);
    // 异常退出路径仍终止进程组；正常关闭已在 worker 中完成 wait。
    if let Some(mut pty) = session.borrow_mut().take() {
        pty.terminate()?;
    }
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("terminal: {error}");
            ExitCode::FAILURE
        }
    }
}
