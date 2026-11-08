//! 资源管理器：标准控件承担交互，文件/导航领域只处理路径、快照与后台副作用。
mod browser;
mod chrome;
mod column_dialog;
mod folder_dialog;
mod model;
mod network_dialog;
mod open_with;
mod session;
mod view;
mod webview;
use browser::{Browser, Visit};
use chrome::{Action, Command};
use desktop_entry::{file_uris, fileops};
use liteos_ui::{
    Application, ControlFlow, EditCommand, EventKind, MessageButtons, MessageResult, WindowOptions,
};
use model::Location;
use session::{Busy, Job, Launches, Session};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    io::{self, Read},
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
    /// shell 的 explore 动词（桌面“资源管理器(&X)”）：打开时显示左侧文件夹树。
    explore: bool,
}
fn parse_launch(home: &Path) -> Result<Launch> {
    let mut location = None;
    let mut select = None;
    let mut verb = None;
    let mut explore = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--explore" {
            explore = true;
        } else if argument == "--select" {
            select = Some(args.next().ok_or("--select needs a name")?);
        } else if argument == "--verb" {
            verb = Some(
                match args.next().as_deref().and_then(|value| value.to_str()) {
                    Some("properties") => Action::Properties,
                    Some("open-with") => Action::OpenWith,
                    // 桌面“我的电脑/网上邻居”右键菜单的网络驱动器命令，与“工具”菜单共用同一向导。
                    Some("map-drive") => Action::MapDrive,
                    Some("disconnect-drive") => Action::DisconnectDrive,
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
        explore,
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
        (44, true, false) if !address => Action::Undo,
        (30, true, _) if !address => Action::SelectAll,
        _ => return None,
    })
}
/// 通过同一消息框入口显示文件操作的独立确认。
fn shell_message(title: &str, text: &str, buttons: MessageButtons) -> Result<MessageResult> {
    let mut app = Application::new()?;
    let dialog = app.standalone_message_box(title, text, buttons)?;
    let result = Rc::new(RefCell::new(None));
    let output = result.clone();
    let _task = app.spawn(async move { *output.borrow_mut() = Some(dialog.await) })?;
    app.run(|_, _| Ok(ControlFlow::Continue))?;
    Ok(result
        .borrow_mut()
        .take()
        .transpose()?
        .unwrap_or(MessageResult::Cancel))
}

enum ShellDelete {
    EmptyRecycleBin,
    Entries {
        paths: Vec<PathBuf>,
        intent: fileops::DeleteIntent,
    },
}

fn delete_from_shell(home: &Path, request: ShellDelete) -> Result<()> {
    let operation = (|| -> Result<()> {
        let (entries, permanent, empty) = match request {
            ShellDelete::EmptyRecycleBin => {
                (fileops::Deletion::prepare_empty_trash(home)?, true, true)
            }
            ShellDelete::Entries { paths, intent } => (
                fileops::Deletion::prepare(home, paths, intent)?,
                intent == fileops::DeleteIntent::Permanent,
                false,
            ),
        };
        if entries.is_empty() {
            return Ok(());
        }
        let (title, text) = session::delete_prompt(&entries, permanent, empty);
        if shell_message(title, &text, MessageButtons::YesNo)? == MessageResult::Yes {
            fileops::Deletion::commit(home, entries, |_| {})?;
        }
        Ok(())
    })();
    if let Err(error) = operation {
        shell_message(
            "删除文件或文件夹时出错",
            &error.to_string(),
            MessageButtons::Ok,
        )?;
    }
    Ok(())
}

fn run() -> Result<()> {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let mut args = std::env::args_os().skip(1);
    match args.next().as_deref().and_then(|arg| arg.to_str()) {
        Some("--empty-recycle-bin") => {
            if args.next().is_some() {
                return Err("empty recycle bin takes no paths".into());
            }
            return delete_from_shell(&home, ShellDelete::EmptyRecycleBin);
        }
        Some(verb @ ("--delete" | "--recycle" | "--delete-permanently")) => {
            let mut paths: Vec<_> = args.map(PathBuf::from).collect();
            if paths.len() == 1 && paths[0] == Path::new("-") {
                // `-` 从标准输入读取同一 URI 列表，避免传输能力受 exec 参数长度限制。
                let mut bytes = Vec::new();
                io::stdin()
                    .take((file_uris::MAX_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)?;
                paths = file_uris::decode(&bytes, file_uris::MAX_PATHS)
                    .ok_or("invalid deletion URI list")?;
            }
            if paths.is_empty()
                || paths.len() > file_uris::MAX_PATHS
                || paths.iter().any(|path| !path.is_absolute())
            {
                return Err("delete requires absolute paths".into());
            }
            return delete_from_shell(
                &home,
                ShellDelete::Entries {
                    paths,
                    intent: match verb {
                        "--recycle" => fileops::DeleteIntent::RecycleOnly,
                        "--delete-permanently" => fileops::DeleteIntent::Permanent,
                        _ => fileops::DeleteIntent::Default,
                    },
                },
            );
        }
        _ => {}
    }
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
    let directory_changes = linux_uapi::filesystem::DirectoryChanges::open().ok();
    let directory_source = directory_changes
        .as_ref()
        .map(|watch| -> Result<_> { Ok(app.watch(watch.descriptor().try_clone_to_owned()?)?) })
        .transpose()?;
    let session = Rc::new(Session {
        view,
        browser: RefCell::new(Browser::new(launch.location.clone())),
        home,
        background,
        dialogs: app.dialogs(),
        clipboard: app.clipboard(),
        busy: Cell::new(true),
        directory_changes: RefCell::new(directory_changes),
        trash_watch: RefCell::new(None),
        undo: RefCell::new(Vec::new()),
        rename_draft: RefCell::new(None),
        closing: Cell::new(false),
        one_shot: launch.verb.is_some(),
        launches: launches.clone(),
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
            if launch.explore {
                guard.0.show_folders()?;
            }
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
                guard.0.closing.set(true);
            }
            Ok::<(), Box<dyn std::error::Error>>(())
        }
        .await;
        if let Err(error) = result {
            guard.0.report("资源管理器", error).await;
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
    let _mounted_views = {
        let session = session.clone();
        let clock = app.clock();
        app.spawn_local(&window, async move {
            loop {
                // 已有内容通知时降低拓扑扫描频率。首次创建、监听失败和网络驱动器仍按秒发现。
                let watched_trash = session.directory_changes.borrow().is_some()
                    && session
                        .trash_watch
                        .borrow()
                        .as_ref()
                        .is_some_and(|(dirs, watched)| *watched && !dirs.is_empty());
                let delay = Duration::from_secs(if watched_trash { 3 } else { 1 });
                let Ok(wait) = clock.sleep(delay) else {
                    break;
                };
                wait.await;
                // Mount changes have no notification endpoint. Discover only for the active view.
                if session.browser.borrow().location == Location::RecycleBin {
                    session.refresh_trash().await;
                } else {
                    let _ = session.refresh_network_drives().await;
                }
            }
        })?
    };
    let _directory_refresh = if let Some(mut source) = directory_source {
        let session = session.clone();
        let clock = app.clock();
        Some(app.spawn_local(&window, async move {
            while source.readable().await.is_ok() {
                while session.busy.get() {
                    let Ok(wait) = clock.sleep(Duration::from_millis(100)) else {
                        return;
                    };
                    wait.await;
                }
                let changed = session
                    .directory_changes
                    .borrow_mut()
                    .as_mut()
                    .map(|watch| watch.drain());
                match changed {
                    Some(Ok(true)) => {
                        session.busy.set(true);
                        let guard = Busy(session.clone());
                        // A removed current directory remains navigable via Up or Back.
                        let _ = guard.0.refresh_external().await;
                    }
                    Some(Ok(false)) => {}
                    _ => break,
                }
            }
            session.directory_changes.borrow_mut().take();
        })?)
    } else {
        None
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
            EventKind::ClipboardChanged => session.menus()?,
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
            EventKind::FileDrag { serial, .. } if event.node == view.list.id() => {
                if let Err(error) = session.start_file_drag(serial) {
                    view.status.set_text(&format!("无法拖动：{error}\t\t"))?;
                }
            }
            EventKind::RenameRequested { row }
                if event.node == view.list.id() && view.list.selection()? == Some(row) =>
            {
                job = Some(Job::Command(Command::Do(Action::Rename)));
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
                job = Some(Job::Context {
                    on_item: target.is_some(),
                    at: None,
                });
            }
            EventKind::ContextMenu { row, at } if event.node == view.list.id() => {
                job = Some(Job::Context {
                    on_item: row.is_some(),
                    at: Some(at),
                });
            }
            EventKind::Key {
                code,
                pressed: true,
                repeat,
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
                    job = match shortcut(code, ctrl, alt, address) {
                        Some(Action::Delete) if repeat || modifiers & 8 != 0 => None,
                        Some(Action::Delete) if modifiers & 1 != 0 => Some(Job::DeletePermanently),
                        action => action.map(|action| Job::Command(Command::Do(action))),
                    };
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
                    guard.0.report("资源管理器", error).await;
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
