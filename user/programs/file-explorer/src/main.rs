//! 资源管理器：标准控件承担交互，文件/导航领域只处理路径、快照与后台副作用。
mod browser;
mod chrome;
mod fileops;
mod model;
mod view;
use browser::{Browser, Clipboard, Visit};
use chrome::{Availability, Command};
use liteos_ui::{
    Application, Background, ControlFlow, Dialogs, EditCommand, EventKind, MessageButtons,
    MessageResult, WindowOptions, state,
};
use model::{Item, Location};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Child, ExitCode},
    rc::Rc,
    task::Waker,
    time::Duration,
};
use view::View;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Default)]
struct Launches {
    children: Vec<Child>,
    wake: Option<Waker>,
}
struct Session {
    view: View,
    browser: RefCell<Browser>,
    home: PathBuf,
    background: Background,
    dialogs: Dialogs,
    busy: Cell<bool>,
    closing: Cell<bool>,
    one_shot: bool,
    launches: Rc<RefCell<Launches>>,
}
enum Action {
    Command(Command),
    Go(Location, Visit, bool),
    Expand(u64),
    Rename(u64, String),
    Context(bool),
}
impl Session {
    fn availability(&self) -> liteos_ui::Result<Availability> {
        let browser = self.browser.borrow();
        Ok(Availability {
            selected: !self.view.list.selected_rows()?.is_empty(),
            editable: matches!(browser.location, Location::Directory(_)),
            paste: browser.clipboard.is_some(),
            trash: matches!(&browser.location,Location::Directory(path)if fileops::in_trash(&self.home,path)),
            toolbar: self.view.toolbar.state()? & state::HIDDEN == 0,
            status: self.view.status.state()? & state::HIDDEN == 0,
            folders: self.view.folders.state()? & state::HIDDEN == 0,
            style: self.view.list.list_style()?,
            busy: self.busy.get(),
        })
    }
    fn menus(&self) -> liteos_ui::Result<()> {
        let availability = self.availability()?;
        self.view.menus(&self.browser.borrow(), &availability)
    }
    fn status(&self) -> liteos_ui::Result<()> {
        let browser = self.browser.borrow();
        let selected = self.view.list.selected_rows()?.len();
        self.view.status.set_text(&format!(
            "{} 项\t选中 {} 项{}",
            browser.items.len(),
            selected,
            if browser.truncated {
                "；目录过大，仅显示前 4096 项"
            } else {
                ""
            }
        ))
    }
    fn selected(&self) -> liteos_ui::Result<Option<Item>> {
        let id = self.view.list.selection()?;
        Ok(id.and_then(|id| self.browser.borrow().item(id).cloned()))
    }
    fn selected_paths(&self) -> liteos_ui::Result<Vec<PathBuf>> {
        let ids = self.view.list.selected_rows()?;
        let browser = self.browser.borrow();
        Ok(ids
            .into_iter()
            .filter_map(|id| browser.item(id)?.path.clone())
            .collect())
    }
    fn record(&self, child: Child) {
        let wake = {
            let mut launches = self.launches.borrow_mut();
            launches.children.push(child);
            launches.wake.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }
    async fn navigate(&self, location: Location, visit: Visit, focus: bool) -> Result<()> {
        self.view.status.set_text("正在读取…")?;
        let home = self.home.clone();
        let listing = self
            .background
            .run(move || browser::read(location, &home))?
            .await?;
        {
            let mut browser = self.browser.borrow_mut();
            browser.publish(listing, visit);
            self.view.rows(&browser)?;
            self.view.tree(&browser)?;
            self.view.address.set_text(&browser.location.address())?;
            self.view.window.set_title(&browser.location.title())?;
        }
        if focus {
            self.view.list.focus()?;
        }
        self.status()?;
        Ok(())
    }
    async fn refresh(&self) -> Result<()> {
        let location = self.browser.borrow().location.clone();
        self.navigate(location, Visit::Refresh, false).await
    }
    fn select_path(&self, path: &Path) -> liteos_ui::Result<Option<u64>> {
        let id = self
            .browser
            .borrow()
            .items
            .iter()
            .find_map(|(id, item)| (item.path.as_deref() == Some(path)).then_some(*id));
        self.view.list.select(id)?;
        Ok(id)
    }
    async fn expand(&self, id: u64) -> Result<()> {
        let location = self
            .browser
            .borrow()
            .catalogue
            .nodes
            .get(&id)
            .map(|node| node.location.clone());
        if let Some(location) = location {
            let branch = self
                .background
                .run(move || browser::read_branch(location))?
                .await?;
            let mut browser = self.browser.borrow_mut();
            browser.catalogue.update(branch);
            self.view.tree(&browser)?;
        }
        Ok(())
    }
    async fn rename(&self, row: u64, name: String) -> Result<()> {
        let item = self
            .browser
            .borrow()
            .item(row)
            .cloned()
            .ok_or("项目已不存在")?;
        let Some(path) = item.path else {
            return Ok(());
        };
        if item.name == name {
            self.view.list.finish_label_edit(true)?;
            if self.one_shot {
                self.closing.set(true);
            }
            return Ok(());
        }
        let result = self
            .background
            .run(move || fileops::rename(&path, &name))?
            .await;
        match result {
            Ok(target) => {
                self.view.list.finish_label_edit(true)?;
                self.refresh().await?;
                self.select_path(&target)?;
                if self.one_shot {
                    self.closing.set(true);
                }
                Ok(())
            }
            Err(error) => {
                self.view.list.finish_label_edit(false)?;
                Err(error.into())
            }
        }
    }
    async fn delete(&self, empty: bool) -> Result<()> {
        let paths = self.selected_paths()?;
        if paths.is_empty() && !empty {
            return Ok(());
        }
        let trash = self.availability()?.trash;
        let message = if empty {
            "确实要清空回收站吗？这些项目将被永久删除。".to_owned()
        } else if trash {
            format!("确实要永久删除选中的 {} 项吗？", paths.len())
        } else {
            format!("确实要把选中的 {} 项放入回收站吗？", paths.len())
        };
        if self
            .dialogs
            .message(
                &self.view.window,
                "确认删除",
                &message,
                MessageButtons::YesNo,
            )?
            .await?
            != MessageResult::Yes
        {
            return Ok(());
        }
        let home = self.home.clone();
        self.view.status.set_text("正在删除…")?;
        let result = self
            .background
            .run(move || {
                if empty {
                    fileops::empty_trash(&home).map(drop)
                } else {
                    paths
                        .iter()
                        .try_for_each(|path| fileops::delete(&home, path))
                }
            })?
            .await;
        self.refresh().await?;
        result?;
        Ok(())
    }
    async fn paste(&self) -> Result<()> {
        let (clipboard, location) = {
            let browser = self.browser.borrow();
            (browser.clipboard.clone(), browser.location.clone())
        };
        let (Some(clipboard), Location::Directory(directory)) = (clipboard, location) else {
            return Ok(());
        };
        self.view.status.set_text("正在复制或移动…")?;
        let cut = clipboard.cut;
        let (result, remaining, last) = self
            .background
            .run(move || {
                let mut last = None;
                for (index, path) in clipboard.paths.iter().enumerate() {
                    let result = if clipboard.cut {
                        fileops::relocate(path, &directory)
                    } else {
                        fileops::copy(path, &directory)
                    };
                    match result {
                        Ok(target) => last = Some(target),
                        Err(error) => return (Err(error), clipboard.paths[index..].to_vec(), last),
                    }
                }
                (Ok(()), Vec::new(), last)
            })?
            .await;
        if cut {
            self.browser.borrow_mut().clipboard = (!remaining.is_empty()).then_some(Clipboard {
                paths: remaining,
                cut: true,
            });
        }
        self.refresh().await?;
        if let Some(path) = last {
            self.select_path(&path)?;
        }
        result?;
        Ok(())
    }
    async fn command(&self, command: Command) -> Result<()> {
        if let Some(style) = command.style() {
            self.view.list.set_list_style(style, true)?;
            return Ok(());
        }
        match command {
            Command::Open => {
                if let Some(item) = self.selected()? {
                    if let Some(target) = item.target {
                        self.navigate(target, Visit::Push, true).await?;
                    } else {
                        let child = self.background.run(move || browser::open(item))?.await?;
                        self.record(child);
                    }
                }
            }
            Command::Properties => {
                if let Some(item) = self.selected()? {
                    let child = self
                        .background
                        .run(move || browser::properties(item))?
                        .await?;
                    self.record(child);
                }
            }
            Command::Close => self.closing.set(true),
            Command::Cut | Command::Copy => {
                let paths = self.selected_paths()?;
                self.browser.borrow_mut().clipboard = (!paths.is_empty()).then_some(Clipboard {
                    paths,
                    cut: command == Command::Cut,
                });
            }
            Command::Paste => self.paste().await?,
            Command::Delete => self.delete(false).await?,
            Command::EmptyTrash => self.delete(true).await?,
            Command::Restore => {
                let paths = self.selected_paths()?;
                let home = self.home.clone();
                let result = self
                    .background
                    .run(move || {
                        paths
                            .iter()
                            .try_for_each(|path| fileops::restore(&home, path).map(drop))
                    })?
                    .await;
                self.refresh().await?;
                result?;
            }
            Command::Rename => {
                if let Some(item) = self.selected()?
                    && item.path.is_some()
                    && let Some(row) = self.view.list.selection()?
                {
                    self.view.list.edit_label(row, &item.name)?;
                }
            }
            Command::NewFolder => {
                let location = self.browser.borrow().location.clone();
                if let Location::Directory(directory) = location {
                    let path = self
                        .background
                        .run(move || fileops::new_folder(&directory))?
                        .await?;
                    self.refresh().await?;
                    if let Some(id) = self.select_path(&path)? {
                        let name = path.file_name().unwrap_or_default().to_string_lossy();
                        self.view.list.edit_label(id, &name)?;
                    }
                }
            }
            Command::SelectAll => {
                let ids = self
                    .browser
                    .borrow()
                    .items
                    .iter()
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>();
                self.view.list.select_rows(&ids)?;
                self.status()?;
            }
            Command::ToggleToolbar | Command::ToggleStatus | Command::ToggleFolders => {
                let widget = match command {
                    Command::ToggleToolbar => &self.view.toolbar,
                    Command::ToggleStatus => &self.view.status,
                    _ => &self.view.folders,
                };
                widget.set_state(widget.state()? ^ state::HIDDEN)?;
            }
            Command::Refresh => self.refresh().await?,
            Command::Back | Command::Forward => {
                let target =
                    self.browser
                        .borrow()
                        .history(if command == Command::Back { -1 } else { 1 });
                if let Some((cursor, location)) = target {
                    self.navigate(location, Visit::History(cursor), true)
                        .await?;
                }
            }
            Command::Up => {
                let target = self.browser.borrow().location.parent();
                if let Some(location) = target {
                    self.navigate(location, Visit::Push, true).await?;
                }
            }
            Command::Address => {
                let text = self.view.address.text()?;
                let location = {
                    let browser = self.browser.borrow();
                    if text == browser.location.address() {
                        return Ok(());
                    }
                    match text.as_str() {
                        "我的电脑" | "::my-computer" => Location::MyComputer,
                        "控制面板" | "::control-panel" => Location::ControlPanel,
                        "网上邻居" | "::network-places" => Location::NetworkPlaces,
                        _ => {
                            let path = PathBuf::from(text);
                            if path.is_relative() {
                                if let Location::Directory(current) = &browser.location {
                                    Location::Directory(current.join(path))
                                } else {
                                    Location::Directory(self.home.join(path))
                                }
                            } else {
                                Location::Directory(path)
                            }
                        }
                    }
                };
                self.navigate(location, Visit::Push, true).await?;
            }
            Command::ViewMenu => {
                let items = chrome::views(self.view.list.list_style()?);
                if let Some(command) = self
                    .dialogs
                    .menu(&self.view.window, &items)?
                    .await?
                    .and_then(Command::from_id)
                    && let Some(style) = command.style()
                {
                    self.view.list.set_list_style(style, true)?;
                }
            }
            Command::About => {
                self.dialogs
                    .message(
                        &self.view.window,
                        "关于 LiteOS 资源管理器",
                        "LiteOS 资源管理器\n浏览文件夹、我的电脑、控制面板与回收站。",
                        MessageButtons::Ok,
                    )?
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }
    async fn execute(&self, action: Action) -> Result<()> {
        match action {
            Action::Command(command) => self.command(command).await,
            Action::Go(location, visit, focus) => self.navigate(location, visit, focus).await,
            Action::Expand(id) => self.expand(id).await,
            Action::Rename(id, name) => self.rename(id, name).await,
            Action::Context(on_item) => {
                let items = chrome::context(&self.availability()?, on_item);
                if let Some(command) = self
                    .dialogs
                    .menu(&self.view.window, &items)?
                    .await?
                    .and_then(Command::from_id)
                {
                    self.command(command).await?;
                }
                Ok(())
            }
        }
    }
    async fn report(&self, error: impl std::fmt::Display) {
        let message = error.to_string();
        let _ = self.view.status.set_text(&message);
        if let Ok(dialog) = self.dialogs.message(
            &self.view.window,
            "资源管理器",
            &message,
            MessageButtons::Ok,
        ) {
            let _ = dialog.await;
        }
    }
}
/// 一个业务事务占用期，UI 仍处理输入和合成；关闭请求等已开始的文件副作用完成后异步兑现。
struct Busy(Rc<Session>);
impl Drop for Busy {
    fn drop(&mut self) {
        self.0.busy.set(false);
        if self.0.closing.get() {
            let _ = self.0.view.window.close();
        } else {
            let _ = self.0.menus();
        }
    }
}
struct Launch {
    location: Location,
    select: Option<OsString>,
    verb: Option<Command>,
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
                    Some("delete") => Command::Delete,
                    Some("rename") => Command::Rename,
                    Some("properties") => Command::Properties,
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
        browser: RefCell::new(Browser::new(&home, launch.location.clone())),
        home,
        background,
        dialogs: app.dialogs(),
        busy: Cell::new(true),
        closing: Cell::new(false),
        one_shot: launch.verb.is_some(),
        launches: launches.clone(),
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
            }
            if let Some(verb) = launch.verb {
                guard.0.command(verb).await?;
                if verb != Command::Rename {
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
        if matches!(event.kind, EventKind::Close) {
            if session.busy.get() {
                session.closing.set(true);
                session.view.status.set_text("等待文件操作完成后关闭…")?;
                return Ok(ControlFlow::PreventDefault);
            }
            return Ok(ControlFlow::Continue);
        }
        if matches!(event.kind, EventKind::Dismiss)
            && event.node == session.view.list.id()
            && session.one_shot
        {
            window.close()?;
            return Ok(ControlFlow::Continue);
        }
        let mut action = None;
        match event.kind {
            EventKind::Select { row } if event.node == session.view.folders.id() => {
                if let Some(location) = session
                    .browser
                    .borrow()
                    .catalogue
                    .nodes
                    .get(&row)
                    .map(|node| node.location.clone())
                    && location != session.browser.borrow().location
                {
                    action = Some(Action::Go(location, Visit::Push, false));
                }
            }
            EventKind::Expand {
                row,
                expanded: true,
            } if event.node == session.view.folders.id() => action = Some(Action::Expand(row)),
            EventKind::Select { .. } if event.node == session.view.list.id() => {
                session.status()?;
                session.menus()?;
            }
            EventKind::Column { column } if event.node == session.view.list.id() => {
                let mut browser = session.browser.borrow_mut();
                browser.sort = if browser.sort.0 == column {
                    (column, !browser.sort.1)
                } else {
                    (column, column == 2 || column == 4)
                };
                browser.sort_items();
                session.view.rows(&browser)?;
            }
            EventKind::LabelEdited { row } if event.node == session.view.list.id() => {
                action = Some(Action::Rename(row, session.view.list.text()?))
            }
            EventKind::Activate { value } => {
                let command = if event.node == session.view.menu.id() {
                    Command::from_id(value)
                } else if event.node == session.view.list.id() {
                    Some(Command::Open)
                } else if event.node == session.view.go.id() {
                    Some(Command::Address)
                } else {
                    session.view.tools.iter().find_map(|(command, button)| {
                        (button.id() == event.node).then_some(*command)
                    })
                };
                action = command.map(Action::Command);
            }
            EventKind::Button {
                code: 0x111,
                pressed: true,
                ..
            } if event.node == session.view.list.id() => {
                let target = session.view.list.pointed_row()?;
                if let Some(id) = target
                    && !session.view.list.selected_rows()?.contains(&id)
                {
                    session.view.list.select(Some(id))?;
                }
                action = Some(Action::Context(target.is_some()));
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
                if (ctrl && code == 38) || code == 62 {
                    session.view.address.focus()?;
                    session.view.address.edit(EditCommand::SelectAll)?;
                } else {
                    let command = match (code, ctrl, alt) {
                        (28, _, true) => Some(Command::Properties),
                        (28, false, false) if event.node == session.view.address.id() => {
                            Some(Command::Address)
                        }
                        (105, false, true) => Some(Command::Back),
                        (106, false, true) => Some(Command::Forward),
                        (14, false, false) => Some(Command::Up),
                        (60, false, false) => Some(Command::Rename),
                        (63, false, false) => Some(Command::Refresh),
                        (111, false, false) => Some(Command::Delete),
                        (45, true, _) => Some(Command::Cut),
                        (46, true, _) => Some(Command::Copy),
                        (47, true, _) => Some(Command::Paste),
                        _ => None,
                    };
                    action = command.map(Action::Command);
                }
            }
            _ => {}
        }
        if let Some(action) = action
            && !session.busy.replace(true)
        {
            session.menus()?;
            let guard = Busy(session.clone());
            tasks.push(app.spawn_local(&window, async move {
                if let Err(error) = guard.0.execute(action).await {
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
