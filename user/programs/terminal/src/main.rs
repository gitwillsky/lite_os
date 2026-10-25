//! 普通用户终端：共享 UI Canvas + 既有 VT 模型 + 标准 Linux PTY；无终端专用协议。
mod input;
mod view;
use input::Input;
use keyboard::Modifiers;
use linux_uapi::pty::{PtySession, WindowSize};
use liteos_ui::{
    Application, ControlFlow, EventKind, Task, Widget, WidgetKind, WidgetSpec, WindowOptions,
};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    io,
    process::ExitCode,
    rc::Rc,
};
use terminal_model::{Grid, Model};
type Session = Rc<RefCell<Option<PtySession>>>;
fn size(columns: usize, rows: usize, scale: i32) -> WindowSize {
    WindowSize {
        columns: columns as u16,
        rows: rows as u16,
        pixel_width: (columns as i32 * view::WIDTH * scale).clamp(0, u16::MAX as i32) as u16,
        pixel_height: (rows as i32 * view::HEIGHT * scale).clamp(0, u16::MAX as i32) as u16,
    }
}
fn resize(
    canvas: &Widget,
    model: &mut Model,
    session: &Session,
    scale: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    let (columns, rows) = view::geometry(canvas)?;
    if model.columns() == columns && model.rows() == rows {
        return Ok(());
    }
    let candidate = model
        .prepare_resize(columns, rows)
        .ok_or_else(|| io::Error::other("terminal grid allocation failed"))?;
    if let Some(session) = session.borrow().as_ref() {
        session.resize(size(columns, rows, scale))?;
    }
    model.commit_resize(candidate);
    Ok(())
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let program = args.next().unwrap_or_else(|| OsString::from("/bin/sh"));
    let arguments = args.collect::<Vec<_>>();
    // 窗口出现前完成初次 exec；此后所有等待、读写和终止回收通过异步任务推进。
    let pty = PtySession::spawn(size(80, 24, 1), &program, &arguments, &[])?;
    let read_fd = pty.as_fd().try_clone_to_owned()?;
    let write_fd = pty.as_fd().try_clone_to_owned()?;
    let session: Session = Rc::new(RefCell::new(Some(pty)));
    let mut app = Application::new()?;
    let window = app.window(WindowOptions::new("终端", "org.liteos.terminal", 648, 420))?;
    let mut spec = WidgetSpec::new(WidgetKind::Canvas, "");
    spec.layout.grow = 1;
    spec.state = liteos_ui::state::TAB_INPUT;
    let canvas = window.widget(spec)?;
    let model =
        Rc::new(RefCell::new(Model::new(80, 24).ok_or_else(|| {
            io::Error::other("terminal grid allocation failed")
        })?));
    let input = Input::new();
    let focused = Rc::new(Cell::new(false));
    let mut reader = app.watch(read_fd)?;
    let mut writer = app.watch(write_fd)?;
    let background = app.background()?;
    let reader_window = window.downgrade();
    let reader_session = session.clone();
    let reader_model = model.clone();
    let reader_input = input.clone();
    let reader_canvas = canvas.clone();
    let reader_focus = focused.clone();
    let _read = app.spawn_local(&window, async move {
        loop {
            if let Err(error) = reader.readable().await {
                eprintln!("terminal: {error}");
                break;
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
                        let mut reply = Vec::new();
                        reader_model
                            .borrow_mut()
                            .feed(&bytes[..count], |bytes| reply.extend_from_slice(bytes));
                        if let Err(error) = reader_input.push(reply) {
                            eprintln!("terminal: reply queue: {error}");
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
            if let Err(error) =
                view::paint(&reader_canvas, &reader_model.borrow(), reader_focus.get())
            {
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
    let mut pointer = (0., 0.);
    let mut anchor = None;
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
                if let Err(error) = resize(&canvas, &mut model.borrow_mut(), &session, scale) {
                    eprintln!("terminal: resize: {error}");
                }
                view::paint(&canvas, &model.borrow(), focused.get())?;
            }
            EventKind::Focus(value) => {
                focused.set(value);
                anchor = None;
                view::paint(&canvas, &model.borrow(), value)?;
            }
            EventKind::Wheel { vertical, .. } => {
                if model
                    .borrow_mut()
                    .scroll_viewport(vertical.saturating_neg().saturating_mul(3))
                {
                    view::paint(&canvas, &model.borrow(), focused.get())?;
                }
            }
            EventKind::Pointer { x, y } => {
                pointer = (x, y);
                if let Some((column, row)) = anchor {
                    let rect = canvas.geometry()?;
                    let x = ((x as i32 - rect.x).max(0) / view::WIDTH) as usize;
                    let y = ((y as i32 - rect.y).max(0) / view::HEIGHT) as usize;
                    model.borrow_mut().set_selection(column, row, x, y);
                    view::paint(&canvas, &model.borrow(), focused.get())?;
                }
            }
            EventKind::Button {
                code: 0x110,
                pressed,
                ..
            } => {
                if pressed && event.node == canvas.id() {
                    let rect = canvas.geometry()?;
                    let column = ((pointer.0 as i32 - rect.x).max(0) / view::WIDTH) as usize;
                    let row = ((pointer.1 as i32 - rect.y).max(0) / view::HEIGHT) as usize;
                    anchor = Some((column, row));
                    model.borrow_mut().clear_selection();
                } else {
                    anchor = None;
                }
                view::paint(&canvas, &model.borrow(), focused.get())?;
            }
            EventKind::Key {
                code,
                pressed: true,
                modifiers,
                text,
                handled,
                ..
            } if !handled => {
                if modifiers & 3 == 3 && code == 46 {
                    if let Some(selection) = model.borrow().selection_projection() {
                        app.set_clipboard(&selection.text)?;
                    }
                } else if modifiers & 3 == 3 && code == 47 {
                    let read = app.clipboard_text(std::time::Duration::from_secs(3))?;
                    let target = input.clone();
                    paste = Some(app.spawn_local(&window, async move {
                        match read.await {
                            Ok(Some(text)) => {
                                if let Err(error) = target.push(text.into_bytes()) {
                                    eprintln!("terminal: paste: {error}");
                                }
                            }
                            Ok(None) => {}
                            Err(error) => eprintln!("terminal: paste: {error}"),
                        }
                    })?);
                } else {
                    let state = Modifiers {
                        shift: modifiers & 1 != 0,
                        control: modifiers & 2 != 0,
                        alt: modifiers & 4 != 0,
                        super_key: modifiers & 8 != 0,
                        caps: false,
                    };
                    let application_cursor = model.borrow().application_cursor_keys();
                    if let Some(bytes) =
                        keyboard::terminal_input(code, true, state, text, application_cursor)
                    {
                        model.borrow_mut().scroll_viewport(i32::MIN);
                        model.borrow_mut().clear_selection();
                        if let Err(error) = input.push(bytes) {
                            eprintln!("terminal: input: {error}");
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(ControlFlow::Continue)
    })?;
    drop(paste);
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
