//! 资源管理器：标准控件承担交互，文件/导航领域只处理路径、快照与后台副作用。
mod browser;
mod chrome;
mod fileops;
mod folder_dialog;
mod model;
mod network_dialog;
mod open_with;
mod session;
mod view;
mod webview;
use browser::{Browser, Visit};
use chrome::{Action, Command};
use liteos_ui::{Application, ControlFlow, EditCommand, EventKind, WindowOptions};
use model::Location;
use session::{Busy, Job, Launches, Session};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitCode,
    rc::Rc,
    time::Duration,
};
use view::{Target, View};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
struct Launch {
    location: Location,
    select: Option<OsString>,
    verb: Option<Action>,
}
fn parse_launch(home: &Path) -> Result<Launch> {
    let mut location = None;
    let mut select = None;
    let mut verb = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--select" {
            select = Some(args.next().ok_or("--select needs a name")?);
        } else if argument == "--verb" {
            verb = Some(
                match args.next().as_deref().and_then(|value| value.to_str()) {
                    Some("delete") => Action::Delete,
                    Some("rename") => Action::Rename,
                    Some("properties") => Action::Properties,
                    Some("open-with") => Action::OpenWith,
                    _ => return Err("unknown --verb".into()),
                },
            );
        } else if location.is_none() {
            location = Some(match argument.to_str() {
                Some(value) if value.starts_with("::") => Location::parse(value),
                _ => Location::Directory(argument.into()),
            });
        } else {
            return Err("unexpected argument".into());
        }
    }
    Ok(Launch {
        location: location.unwrap_or_else(|| Location::Directory(home.to_owned())),
        select,
        verb,
    })
}
/// 键盘快捷键（Windows 2000 资源管理器）。
fn shortcut(code: u32, ctrl: bool, alt: bool, address: bool) -> Option<Action> {
    Some(match (code, ctrl, alt) {
        (28, _, true) => Action::Properties,
        (28, false, false) if address => Action::Address,
        (105, false, true) => Action::Back,
        (106, false, true) => Action::Forward,
        (14, false, false) if !address => Action::Up,
        (60, false, false) => Action::Rename,
        (63, false, false) => Action::Refresh,
        (111, false, false) if !address => Action::Delete,
        (45, true, _) if !address => Action::Cut,
        (46, true, _) if !address => Action::Copy,
        (47, true, _) if !address => Action::Paste,
        (30, true, _) if !address => Action::SelectAll,
        _ => return None,
    })
}
fn run() -> Result<()> {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let launch = parse_launch(&home)?;
    let mut app = Application::new()?;
    let window = app.window(WindowOptions::new(
        &launch.location.title(),
        "org.liteos.file-explorer",
        760,
        500,
    ))?;
    let view = View::new(window.clone())?;
    let background = app.background()?;
    let launches = Rc::new(RefCell::new(Launches::default()));
    let session = Rc::new(Session {
        view,
        browser: RefCell::new(Browser::new(launch.location.clone())),
        home,
        background,
        dialogs: app.dialogs(),
        busy: Cell::new(true),
        closing: Cell::new(false),
        one_shot: launch.verb.is_some(),
        launches: launches.clone(),
        views: RefCell::new(HashMap::new()),
        shown: Cell::new(false),
    });
    session.menus()?;
    let mut tasks = Vec::new();
    let guard = Busy(session.clone());
    let startup = app.spawn_local(&window, async move {
        let result = async {
            guard
                .0
                .navigate(launch.location, Visit::Refresh, true)
                .await?;
            if let Some(name) = launch.select {
                let id = guard
                    .0
                    .browser
                    .borrow()
                    .items
                    .iter()
                    .find_map(|(id, item)| {
                        ((item.path.as_ref().and_then(|path| path.file_name())
                            == Some(name.as_os_str()))
                            || item.name == name.to_string_lossy())
                        .then_some(*id)
                    });
                guard.0.view.list.select(id)?;
                guard.0.status()?;
            }
            if let Some(verb) = launch.verb {
                guard.0.command(Command::Do(verb)).await?;
                if verb != Action::Rename {
                    guard.0.closing.set(true);
                }
            }
            Ok::<(), Box<dyn std::error::Error>>(())
        }
        .await;
        if let Err(error) = result {
            guard.0.report(error).await;
        }
    })?;
    let _reaper = {
        let clock = app.clock();
        app.spawn_local(&window, async move {
            loop {
                std::future::poll_fn(|cx| {
                    let mut launches = launches.borrow_mut();
                    if launches.children.is_empty() {
                        launches.wake = Some(cx.waker().clone());
                        std::task::Poll::Pending
                    } else {
                        std::task::Poll::Ready(())
                    }
                })
                .await;
                launches
                    .borrow_mut()
                    .children
                    .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
                let Ok(wait) = clock.sleep(Duration::from_secs(1)) else {
                    break;
                };
                wait.await;
            }
        })?
    };
    app.run(|app, event| {
        tasks.retain(|task: &liteos_ui::Task<()>| !task.is_finished());
        if event.window != window.id() {
            return Ok(ControlFlow::Continue);
        }
        let view = &session.view;
        if matches!(event.kind, EventKind::Close) {
            if session.busy.get() {
                session.closing.set(true);
                view.status.set_text("等待文件操作完成后关闭…\t\t")?;
                return Ok(ControlFlow::PreventDefault);
            }
            return Ok(ControlFlow::Continue);
        }
        if matches!(event.kind, EventKind::Dismiss)
            && event.node == view.list.id()
            && session.one_shot
        {
            window.close()?;
            return Ok(ControlFlow::Continue);
        }
        let mut job = None;
        match event.kind {
            EventKind::Configure { .. } => view.fit_panel()?,
            EventKind::Select { row }
                if event.node == view.folders.id() || event.node == view.address.id() =>
            {
                let browser = session.browser.borrow();
                if let Some(location) = browser
                    .catalogue
                    .nodes
                    .get(&row)
                    .map(|node| node.location.clone())
                    && location != browser.location
                {
                    job = Some(Job::Go(
                        location,
                        Visit::Push,
                        event.node == view.address.id(),
                    ));
                }
            }
            EventKind::Expand {
                row,
                expanded: true,
            } if event.node == view.folders.id() => job = Some(Job::Expand(row)),
            EventKind::Select { .. } if event.node == view.list.id() => {
                session.status()?;
                session.menus()?;
            }
            EventKind::Column { column } if event.node == view.list.id() => {
                let Some(column) = model::Field::from_id(column) else {
                    return Ok(ControlFlow::Continue);
                };
                let mut browser = session.browser.borrow_mut();
                browser.sort = if browser.sort.0 == column {
                    (column, !browser.sort.1)
                } else {
                    (column, column.numeric())
                };
                browser.sort_items();
                view.rows(&browser)?;
            }
            EventKind::LabelEdited { row } if event.node == view.list.id() => {
                job = Some(Job::Rename(row, view.list.text()?))
            }
            EventKind::Dropdown { button, rect } if event.node == view.toolbar.id() => {
                if let Some(Command::Do(action)) = Command::from_id(button) {
                    job = Some(Job::Dropdown(action, (rect.x, rect.bottom())));
                }
            }
            EventKind::Activate { value } => {
                let command = if event.node == view.list.id() {
                    Some(Command::Do(Action::Open))
                } else if event.node == view.address.id() {
                    Some(Command::Do(Action::Address))
                } else if event.node == view.folders_close.id() {
                    Some(Command::Do(Action::ToggleFolders))
                } else if let Some(target) = view.target(event.node) {
                    match target {
                        Target::Place(location) => {
                            job = Some(Job::Go(location, Visit::Push, true));
                            None
                        }
                        Target::Do(action) => Some(Command::Do(action)),
                    }
                } else {
                    view.command_of(event.node, value)
                };
                if let Some(command) = command {
                    job = Some(Job::Command(command));
                }
            }
            EventKind::Button {
                code: 0x111,
                pressed: true,
                ..
            } if event.node == view.list.id() => {
                let target = view.list.pointed_row()?;
                if let Some(id) = target
                    && !view.list.selected_rows()?.contains(&id)
                {
                    view.list.select(Some(id))?;
                    session.status()?;
                }
                job = Some(Job::Context(target.is_some()));
            }
            EventKind::Key {
                code,
                pressed: true,
                modifiers,
                handled: false,
                ..
            } => {
                let ctrl = modifiers & 2 != 0;
                let alt = modifiers & 4 != 0;
                // Alt+D 与 F4 转到地址栏（F4 在组合框内由控件展开下拉列表）。
                if (alt && code == 32) || (code == 62 && event.node != view.address.id()) {
                    view.address.focus()?;
                    view.address.edit(EditCommand::SelectAll)?;
                } else {
                    let address = event.node == view.address.id();
                    job = shortcut(code, ctrl, alt, address)
                        .map(|action| Job::Command(Command::Do(action)));
                }
            }
            _ => {}
        }
        if let Some(job) = job
            && !session.busy.replace(true)
        {
            session.menus()?;
            let guard = Busy(session.clone());
            tasks.push(app.spawn_local(&window, async move {
                if let Err(error) = guard.0.execute(job).await {
                    guard.0.report(error).await;
                }
            })?);
        }
        Ok(ControlFlow::Continue)
    })?;
    drop(startup);
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("file-explorer: {error}");
            ExitCode::FAILURE
        }
    }
}
