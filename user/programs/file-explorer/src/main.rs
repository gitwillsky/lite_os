//! 资源管理器：标准控件承担交互，文件/导航领域只处理路径、快照与后台副作用。
mod browser;
mod chrome;
mod fileops;
mod folder_dialog;
mod model;
mod view;
mod webview;
use browser::{Browser, Clipboard, Visit};
use chrome::{Action, Availability, Command};
use liteos_ui::{
    Application, Background, ControlFlow, Dialogs, EditCommand, EventKind, ListStyle,
    MessageButtons, MessageResult, WindowOptions, state,
};
use model::{Item, Location};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Child, ExitCode},
    rc::Rc,
    task::Waker,
    time::Duration,
};
use view::{Target, View};
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
    // OWNER: 本窗口独占每个位置最后使用的查看方式（Windows 2000“记住每个文件夹的视图设置”）。
    // FAILURE: 缺少它切换位置后总回到默认大图标，用户为某个文件夹选的详细资料等方式会丢失。
    views: RefCell<HashMap<Location, ListStyle>>,
    /// 是否已经显示过一个位置；之前列表的查看方式只是控件初始值，不能当作上次的选择记录。
    shown: Cell<bool>,
}
/// 一次占用窗口的业务事务。
enum Job {
    Command(Command),
    Go(Location, Visit, bool),
    Expand(u64),
    Rename(u64, String),
    Context(bool),
    /// 工具栏下拉按钮：（按钮命令，菜单左上角）。
    Dropdown(Action, (i32, i32)),
}
impl Session {
    fn availability(&self) -> liteos_ui::Result<Availability> {
        let browser = self.browser.borrow();
        let hidden = |widget: &liteos_ui::Widget| -> liteos_ui::Result<bool> {
            Ok(widget.state()? & state::HIDDEN != 0)
        };
        let trash = browser.location.is_recycle_bin();
        Ok(Availability {
            selected: !self.view.list.selected_rows()?.is_empty(),
            editable: matches!(browser.location, Location::Directory(_)),
            paste: browser.clipboard.is_some(),
            trash,
            trash_items: trash && !browser.items.is_empty(),
            standard_buttons: !hidden(&self.view.toolbar)?,
            address_bar: !hidden(&self.view.address_band)?,
            status: !hidden(&self.view.status)?,
            folders: !hidden(&self.view.folders_pane)?,
            style: self.view.list.list_style()?,
            busy: self.busy.get(),
            back: browser.history(-1).is_some(),
            forward: browser.history(1).is_some(),
            up: browser.location.parent().is_some(),
            columns: browser
                .location
                .columns()
                .iter()
                .map(|(field, title, _)| (*field, *title))
                .collect(),
            sort: browser.sort.0,
            history: browser.journal(),
            connection: self
                .view
                .list
                .selection()?
                .and_then(|id| browser.item(id))
                .and_then(|item| match &item.details {
                    model::Details::Connection { enabled, .. } => Some(*enabled),
                    _ => None,
                }),
        })
    }
    fn menus(&self) -> liteos_ui::Result<()> {
        let availability = self.availability()?;
        self.view.menus(&availability)
    }
    /// 选择变化后的状态栏与 Web 视图。
    fn status(&self) -> liteos_ui::Result<()> {
        let browser = self.browser.borrow();
        let selected = self.view.list.selected_rows()?;
        self.view.status(&browser, &selected, browser.free)?;
        self.view.web_view(&browser, &self.home, &selected)
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
    /// 选中的网络连接：（接口名，是否启用）。
    fn connection(&self) -> liteos_ui::Result<Option<(String, bool)>> {
        Ok(self.selected()?.and_then(|item| match item.details {
            model::Details::Connection {
                interface, enabled, ..
            } => Some((interface, enabled)),
            _ => None,
        }))
    }
    /// 启用或禁用连接，等 network-service 把接口切到目标状态（最多 3 秒）后刷新列表。
    async fn set_connection(&self, interface: String, enabled: bool) -> Result<()> {
        self.view.status.set_text(if enabled {
            "正在启用…\t\t"
        } else {
            "正在禁用…\t\t"
        })?;
        self.background
            .run(move || {
                network_config::set_enabled(&interface, enabled)?;
                let flags = || {
                    std::fs::read_to_string(format!("/sys/class/net/{interface}/flags"))
                        .ok()
                        .and_then(|text| {
                            u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
                        })
                        .unwrap_or(0)
                };
                for _ in 0..30 {
                    if (flags() & 1 != 0) == enabled {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok::<(), std::io::Error>(())
            })?
            .await?;
        self.refresh().await
    }
    fn launch(&self, program: &str, arguments: &[&str]) -> Result<()> {
        let child = std::process::Command::new(program)
            .args(arguments)
            .env_remove("WAYLAND_SOCKET")
            .spawn()?;
        self.record(child);
        Ok(())
    }
    async fn navigate(&self, location: Location, visit: Visit, focus: bool) -> Result<()> {
        self.view.status.set_text("正在读取…\t\t")?;
        // 品牌 logo 在读取期间转动（browseui 的 throbber）。
        self.view.rebar.set_busy(true)?;
        let home = self.home.clone();
        let result = self
            .background
            .run(move || browser::read(location, &home))?
            .await;
        self.view.rebar.set_busy(false)?;
        let listing = result?;
        {
            let mut browser = self.browser.borrow_mut();
            // 只记录已经显示过的位置；首次读取前列表还是控件初始样式，不是用户的选择。
            if self.shown.replace(true) {
                let style = self.view.list.list_style()?;
                self.views
                    .borrow_mut()
                    .insert(browser.location.clone(), style);
            }
            browser.publish(listing, visit);
            // Windows 2000 文件夹的默认查看方式是大图标；访问过的位置沿用上次的方式。
            let style = self
                .views
                .borrow()
                .get(&browser.location)
                .copied()
                .unwrap_or(ListStyle::LargeIcons);
            self.view.list.set_list_style(style, view::MULTIPLE)?;
            self.view.rows(&browser)?;
            self.view.tree(&browser)?;
            self.view.window.set_title(&browser.location.title())?;
        }
        if focus {
            self.view.list.focus()?;
        }
        self.view.fit_panel()?;
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
        // shell32 67/68 与永久删除确认。
        let message = if empty {
            "确实要永久删除“回收站”中的所有项目吗?".to_owned()
        } else if trash {
            format!("确实要永久删除这 {} 项吗?", paths.len())
        } else if paths.len() == 1 {
            "确实要将它移动到回收站吗?".to_owned()
        } else {
            format!("确实要将这 {} 项移动到回收站吗?", paths.len())
        };
        let title = if paths.len() > 1 {
            "确认删除多个文件"
        } else {
            "确认文件删除"
        };
        if self
            .dialogs
            .message(&self.view.window, title, &message, MessageButtons::YesNo)?
            .await?
            != MessageResult::Yes
        {
            return Ok(());
        }
        let home = self.home.clone();
        self.view.status.set_text("正在删除…\t\t")?;
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
    /// 复制或移动到目录；返回最后一个目标路径。剪切的剩余项留在剪贴板。
    async fn transfer(&self, paths: Vec<PathBuf>, directory: PathBuf, cut: bool) -> Result<()> {
        self.view.status.set_text("正在复制或移动…\t\t")?;
        let (result, remaining, last) = self
            .background
            .run(move || {
                let mut last = None;
                for (index, path) in paths.iter().enumerate() {
                    let result = if cut {
                        fileops::relocate(path, &directory)
                    } else {
                        fileops::copy(path, &directory)
                    };
                    match result {
                        Ok(target) => last = Some(target),
                        Err(error) => return (Err(error), paths[index..].to_vec(), last),
                    }
                }
                (Ok(()), Vec::new(), last)
            })?
            .await;
        if cut
            && self
                .browser
                .borrow()
                .clipboard
                .as_ref()
                .is_some_and(|clipboard| clipboard.cut)
        {
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
    async fn paste(&self) -> Result<()> {
        let (clipboard, location) = {
            let browser = self.browser.borrow();
            (browser.clipboard.clone(), browser.location.clone())
        };
        let (Some(clipboard), Location::Directory(directory)) = (clipboard, location) else {
            return Ok(());
        };
        self.transfer(clipboard.paths, directory, clipboard.cut)
            .await
    }
    /// “复制到文件夹”/“移动到文件夹”：用共享的“浏览文件夹”对话框选目标（shell32 30306/30307）。
    async fn copy_to(&self, cut: bool) -> Result<()> {
        let paths = self.selected_paths()?;
        if paths.is_empty() {
            return Ok(());
        }
        let prompt = if cut {
            "将选定项目移动到文件夹:"
        } else {
            "将选定项目复制到文件夹:"
        };
        let title = if cut { "移动项目" } else { "复制项目" };
        let start = match &self.browser.borrow().location {
            Location::Directory(path) => path.clone(),
            _ => self.home.clone(),
        };
        let Some(directory) = folder_dialog::browse(
            &self.dialogs,
            &self.background,
            &self.view.window,
            title,
            prompt,
            &start,
        )
        .await?
        else {
            return Ok(());
        };
        self.transfer(paths, directory, cut).await
    }
    async fn create(&self, folder: bool) -> Result<()> {
        let location = self.browser.borrow().location.clone();
        let Location::Directory(directory) = location else {
            return Ok(());
        };
        let path = self
            .background
            .run(move || {
                if folder {
                    fileops::new_folder(&directory)
                } else {
                    fileops::new_file(&directory, "新建 文本文档.txt")
                }
            })?
            .await?;
        self.refresh().await?;
        if let Some(id) = self.select_path(&path)? {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            self.view.list.edit_label(id, &name)?;
        }
        Ok(())
    }
    async fn restore(&self, paths: Vec<PathBuf>) -> Result<()> {
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
        Ok(())
    }
    fn toggle(&self, widget: &liteos_ui::Widget) -> liteos_ui::Result<()> {
        widget.set_state(widget.state()? ^ state::HIDDEN)?;
        self.view.fit_panel()
    }
    async fn command(&self, command: Command) -> Result<()> {
        if let Some(style) = command.style() {
            self.view.list.set_list_style(style, view::MULTIPLE)?;
            return Ok(());
        }
        let action = match command {
            Command::Arrange(field) => {
                let mut browser = self.browser.borrow_mut();
                browser.sort = (field, field.numeric());
                browser.sort_items();
                self.view.rows(&browser)?;
                return Ok(());
            }
            Command::Visit(index) => {
                let target = self.browser.borrow().history(0).and_then(|(cursor, _)| {
                    self.browser
                        .borrow()
                        .history(index as isize - cursor as isize)
                });
                if let Some((cursor, location)) = target {
                    self.navigate(location, Visit::History(cursor), true)
                        .await?;
                }
                return Ok(());
            }
            Command::Do(action) => action,
        };
        match action {
            Action::Open if self.connection()?.is_some_and(|(_, enabled)| !enabled) => {
                // 已禁用连接的默认动词是“启用”。
                let (interface, _) = self.connection()?.expect("connection");
                self.set_connection(interface, true).await?;
            }
            Action::Enable | Action::Disable => {
                if let Some((interface, _)) = self.connection()? {
                    self.set_connection(interface, action == Action::Enable)
                        .await?;
                }
            }
            Action::Status => {
                if let Some((interface, _)) = self.connection()? {
                    self.launch("/bin/control-panel", &["network", &interface])?;
                }
            }
            Action::Properties if self.connection()?.is_some() => {
                let (interface, _) = self.connection()?.expect("connection");
                self.launch("/bin/control-panel", &["connection", &interface])?;
            }
            Action::Open => {
                if let Some(item) = self.selected()? {
                    if let Some(target) = item.target {
                        self.navigate(target, Visit::Push, true).await?;
                    } else {
                        let child = self.background.run(move || browser::open(item))?.await?;
                        self.record(child);
                    }
                }
            }
            Action::Properties => {
                if let Some(item) = self.selected()? {
                    let child = self
                        .background
                        .run(move || browser::properties(item))?
                        .await?;
                    self.record(child);
                }
            }
            Action::Close => self.closing.set(true),
            Action::Cut | Action::Copy => {
                let paths = self.selected_paths()?;
                self.browser.borrow_mut().clipboard = (!paths.is_empty()).then_some(Clipboard {
                    paths,
                    cut: action == Action::Cut,
                });
            }
            Action::Paste => self.paste().await?,
            Action::CopyTo => self.copy_to(false).await?,
            Action::MoveTo => self.copy_to(true).await?,
            Action::SendToDocuments => {
                let paths = self.selected_paths()?;
                let home = self.home.clone();
                self.transfer(paths, home, false).await?;
            }
            Action::Delete => self.delete(false).await?,
            Action::EmptyTrash => self.delete(true).await?,
            Action::Restore => {
                let paths = self.selected_paths()?;
                self.restore(paths).await?;
            }
            Action::RestoreAll => {
                let paths = self
                    .browser
                    .borrow()
                    .items
                    .iter()
                    .filter_map(|(_, item)| item.path.clone())
                    .collect();
                self.restore(paths).await?;
            }
            Action::Rename => {
                if let Some(item) = self.selected()?
                    && item.path.is_some()
                    && let Some(row) = self.view.list.selection()?
                {
                    self.view.list.edit_label(row, &item.name)?;
                }
            }
            Action::NewFolder => self.create(true).await?,
            Action::NewTextDocument => self.create(false).await?,
            Action::SelectAll | Action::InvertSelection => {
                let selected = self.view.list.selected_rows()?;
                let ids = self
                    .browser
                    .borrow()
                    .items
                    .iter()
                    .map(|(id, _)| *id)
                    .filter(|id| action == Action::SelectAll || !selected.contains(id))
                    .collect::<Vec<_>>();
                self.view.list.select_rows(&ids)?;
                self.status()?;
            }
            Action::ToggleStandardButtons => self.toggle(&self.view.toolbar)?,
            Action::ToggleAddressBar => self.toggle(&self.view.address_band)?,
            Action::ToggleStatus => self.toggle(&self.view.status)?,
            Action::ToggleFolders => self.toggle(&self.view.folders_pane)?,
            Action::Refresh => self.refresh().await?,
            Action::Back | Action::Forward => {
                let target =
                    self.browser
                        .borrow()
                        .history(if action == Action::Back { -1 } else { 1 });
                if let Some((cursor, location)) = target {
                    self.navigate(location, Visit::History(cursor), true)
                        .await?;
                }
            }
            Action::Up => {
                let target = self.browser.borrow().location.parent();
                if let Some(location) = target {
                    self.navigate(location, Visit::Push, true).await?;
                }
            }
            Action::Address => {
                let text = self.view.address.text()?;
                let location = {
                    let browser = self.browser.borrow();
                    if text == browser.location.address() {
                        return Ok(());
                    }
                    match Location::parse(&text) {
                        Location::Directory(path) if path.is_relative() => {
                            if let Location::Directory(current) = &browser.location {
                                Location::Directory(current.join(path))
                            } else {
                                Location::Directory(self.home.join(path))
                            }
                        }
                        location => location,
                    }
                };
                self.navigate(location, Visit::Push, true).await?;
            }
            Action::AboutWindows => self.launch("/bin/control-panel", &["winver"])?,
            // 其余命令在菜单与工具栏中保持禁用（没有对应后端）。
            _ => {}
        }
        Ok(())
    }
    /// 工具栏下拉：后退/前进列出旅行日志，“查看”列出查看方式；菜单贴在按钮下方。
    async fn dropdown(&self, action: Action, at: (i32, i32)) -> Result<()> {
        let entries = match action {
            Action::Back => chrome::travel_menu(&self.browser.borrow().travel(-1)),
            Action::Forward => chrome::travel_menu(&self.browser.borrow().travel(1)),
            Action::ViewMenu => chrome::view_menu(self.view.list.list_style()?),
            _ => return Ok(()),
        };
        if entries.is_empty() {
            return Ok(());
        }
        let items: Vec<_> = entries.iter().map(chrome::Entry::item).collect();
        if let Some(command) = self
            .dialogs
            .menu_at(&self.view.window, &items, at)?
            .await?
            .and_then(Command::from_id)
        {
            self.command(command).await?;
        }
        Ok(())
    }
    async fn execute(&self, job: Job) -> Result<()> {
        match job {
            Job::Command(command) => self.command(command).await,
            Job::Go(location, visit, focus) => self.navigate(location, visit, focus).await,
            Job::Expand(id) => self.expand(id).await,
            Job::Rename(id, name) => self.rename(id, name).await,
            Job::Dropdown(action, at) => self.dropdown(action, at).await,
            Job::Context(on_item) => {
                let entries = chrome::context(&self.availability()?, on_item);
                let items: Vec<_> = entries.iter().map(chrome::Entry::item).collect();
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
        let _ = self.view.status.set_text(&format!("{message}\t\t"));
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
