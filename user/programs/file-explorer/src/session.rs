//! 一个窗口的导航、文件事务、子进程与关闭生命周期。
use crate::{
    Result,
    browser::{self, Browser, Visit},
    chrome::{self, Action, Availability, Command},
    fileops, folder_dialog,
    model::{self, Item, Location},
    open_with,
    view::{self, View},
};
use desktop_entry::file_uris::{self, SELECTION_TYPES};
use desktop_settings::{FolderView, Settings};
use liteos_ui::{Background, Clipboard, Dialogs, ListStyle, MessageButtons, MessageResult, state};
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    process::Child,
    rc::Rc,
    task::Waker,
    time::Duration,
};
#[derive(Default)]
pub(super) struct Launches {
    pub(super) children: Vec<Child>,
    pub(super) wake: Option<Waker>,
}
pub(super) struct Session {
    pub(super) view: View,
    pub(super) browser: RefCell<Browser>,
    pub(super) home: PathBuf,
    pub(super) background: Background,
    pub(super) dialogs: Dialogs,
    pub(super) clipboard: Clipboard,
    pub(super) busy: Cell<bool>,
    /// 单窗口的真实目录监听。目录变更经文件系统唤醒，避免靠拖放结果猜测源文件何时消失。
    pub(super) directory_changes: RefCell<Option<linux_uapi::filesystem::DirectoryChanges>>,
    /// History belongs to this Explorer window and expires with it.
    pub(super) undo: RefCell<Vec<fileops::Undo>>,
    pub(super) closing: Cell<bool>,
    pub(super) one_shot: bool,
    pub(super) launches: Rc<RefCell<Launches>>,
    /// 首次发布列表前，后台刷新不能替换尚未打开的位置。
    pub(super) shown: Cell<bool>,
}

/// Bound the number of completed actions retained by each window.
const UNDO_LIMIT: usize = 16;

/// 一次占用窗口的业务事务。
pub(super) enum Job {
    Command(Command),
    DeletePermanently,
    Go(Location, Visit, bool),
    Expand(u64),
    Rename(u64, String),
    Context {
        on_item: bool,
        at: Option<(i32, i32)>,
    },
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
            file_selected: self.view.list.selected_rows()?.len() == 1
                && self.selected()?.is_some_and(|item| {
                    item.path.is_some()
                        && item.target.is_none()
                        && item.launch.is_none()
                        && item.drive.is_none()
                }),
            editable: matches!(browser.location, Location::Directory(_)),
            paste: matches!(browser.location, Location::Directory(_) | Location::Desktop)
                && self.file_selection()?.is_some(),
            undo: self.undo.borrow().last().map(fileops::Undo::label),
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
    async fn open_with(&self, path: PathBuf) -> Result<()> {
        let associations = self
            .background
            .run(|| Ok::<_, std::io::Error>(desktop_entry::Associations::load()))?
            .await?;
        let Some((entry, always)) =
            open_with::choose(&self.dialogs, &self.view.window, &path, &associations).await?
        else {
            return Ok(());
        };
        let child = self
            .background
            .run(move || {
                if always {
                    associations.set_default(&path, &entry)?;
                }
                entry
                    .command_for(&path)
                    .ok_or(std::io::ErrorKind::InvalidInput)?
                    .env_remove("WAYLAND_SOCKET")
                    .spawn()
            })?
            .await?;
        self.record(child);
        if always {
            self.refresh().await?;
        }
        Ok(())
    }

    pub(super) fn menus(&self) -> liteos_ui::Result<()> {
        let availability = self.availability()?;
        self.view.menus(&availability)
    }
    /// 选择变化后的状态栏与 Web 视图。
    pub(super) fn status(&self) -> liteos_ui::Result<()> {
        let browser = self.browser.borrow();
        let selected = self.view.list.selected_rows()?;
        self.view.status(&browser, &selected, browser.free)?;
        self.view.web_view(&browser, &self.home, &selected)
    }
    fn selected(&self) -> liteos_ui::Result<Option<Item>> {
        let id = self.view.list.selection()?;
        Ok(id.and_then(|id| self.browser.borrow().item(id).cloned()))
    }

    pub(super) fn begin_rename(&self, row: u64) -> liteos_ui::Result<()> {
        let browser = self.browser.borrow();
        if self.view.list.selection()? == Some(row)
            && let Some(item) = browser.item(row)
            && match browser.location {
                Location::Directory(_) | Location::Desktop => {
                    !browser.location.is_recycle_bin() && item.path.is_some()
                }
                Location::NetworkConnections => {
                    matches!(item.details, model::Details::Connection { .. })
                }
                _ => false,
            }
        {
            self.view.list.edit_label(row, &item.name)?;
        }
        Ok(())
    }
    fn selected_paths(&self) -> liteos_ui::Result<Vec<PathBuf>> {
        let ids = self.view.list.selected_rows()?;
        let browser = self.browser.borrow();
        Ok(ids
            .into_iter()
            .filter_map(|id| browser.item(id)?.path.clone())
            .collect())
    }
    pub(super) fn start_file_drag(&self, serial: u32) -> liteos_ui::Result<()> {
        if self.busy.get() {
            return Ok(());
        }
        let browser = self.browser.borrow();
        if !matches!(browser.location, Location::Directory(_)) || browser.location.is_recycle_bin()
        {
            return Ok(());
        }
        drop(browser);
        let paths = self.selected_paths()?;
        if !paths.is_empty() {
            self.view.window.start_file_drag(serial, &paths)?;
        }
        Ok(())
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
    /// 启用或禁用连接，等 netcfg 把接口切到目标状态（最多 3 秒）后刷新列表。
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
    pub(super) async fn navigate(
        &self,
        location: Location,
        visit: Visit,
        focus: bool,
    ) -> Result<()> {
        self.view.status.set_text("正在读取…\t\t")?;
        // 品牌 logo 在读取期间转动（browseui 的 throbber）。
        self.view.rebar.set_busy(true)?;
        // F5 和目录变更通知保留当前窗口的样式。真正进入位置才读取已保存的值。
        let read_view = !matches!(visit, Visit::Refresh) || !self.shown.get();
        let result = self
            .background
            .run(move || {
                let listing = browser::read(location)?;
                let view = read_view.then(|| {
                    listing
                        .location
                        .view_key()
                        .ok()
                        .map(|key| {
                            let settings = Settings::load();
                            (
                                settings.folder_view(&key),
                                settings.folder_columns(&key).to_vec(),
                            )
                        })
                        .unwrap_or_default()
                });
                Ok::<_, std::io::Error>((listing, view))
            })?
            .await;
        self.view.rebar.set_busy(false)?;
        let (listing, view) = result?;
        if let Some(changes) = self.directory_changes.borrow_mut().as_mut() {
            let desktop;
            let path = match &listing.location {
                Location::Directory(path) => Some(path.as_path()),
                Location::Desktop => {
                    desktop = self.home.join("Desktop");
                    Some(desktop.as_path())
                }
                _ => None,
            };
            let _ = changes.follow(path);
        }
        {
            let mut browser = self.browser.borrow_mut();
            browser.publish(listing, visit);
            if let Some((view, columns)) = view {
                browser.set_column_layout(&columns);
                // 新位置使用它保存的宽度。目录内刷新仍保留本窗口手动拖动后的宽度。
                self.view.list.set_columns(&[])?;
                let style = match view {
                    FolderView::LargeIcons => ListStyle::LargeIcons,
                    FolderView::SmallIcons => ListStyle::SmallIcons,
                    FolderView::List => ListStyle::List,
                    FolderView::Details => ListStyle::Details,
                };
                self.view.list.set_list_style(style, view::MULTIPLE)?;
            }
            self.view.rows(&browser)?;
            self.view.tree(&browser)?;
            self.view.window.set_title(&browser.location.title())?;
            self.shown.set(true);
        }
        if focus {
            self.view.list.focus()?;
        }
        self.view.fit_panel()?;
        self.status()?;
        Ok(())
    }
    /// 只读取本地映射记录与 mount table；后台连接完成后无需导航即可更新图标。
    pub(super) async fn refresh_network_drives(&self) -> Result<()> {
        if self.busy.get()
            || !self.shown.get()
            || self.browser.borrow().location != Location::MyComputer
        {
            return Ok(());
        }
        let previous = self.browser.borrow().network_drives();
        let drives = self.background.run(model::network_drives)?.await?;
        // 等待期间可能导航、打开对话框或完成另一项映射操作；旧结果不能覆盖较新的 UI。
        if self.busy.get() || self.browser.borrow().location != Location::MyComputer {
            return Ok(());
        }
        {
            let mut browser = self.browser.borrow_mut();
            if browser.network_drives() != previous || drives == previous {
                return Ok(());
            }
            browser.publish_network_drives(drives);
            self.view.rows(&browser)?;
            self.view.places(&browser)?;
        }
        self.status()?;
        self.menus()?;
        Ok(())
    }

    pub(super) async fn refresh(&self) -> Result<()> {
        let location = self.browser.borrow().location.clone();
        self.navigate(location, Visit::Refresh, false).await
    }
    pub(super) async fn refresh_external(&self) -> Result<()> {
        if !matches!(
            self.browser.borrow().location,
            Location::Directory(_) | Location::Desktop | Location::RecycleBin
        ) {
            return Ok(());
        }
        let paths = self.selected_paths()?;
        self.refresh().await?;
        if !paths.is_empty() {
            let rows = self
                .browser
                .borrow()
                .items
                .iter()
                .filter_map(|(id, item)| {
                    item.path
                        .as_ref()
                        .is_some_and(|path| paths.contains(path))
                        .then_some(*id)
                })
                .collect::<Vec<_>>();
            self.view.list.select_rows(&rows)?;
            self.status()?;
        }
        Ok(())
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
        if item.name == name {
            self.view.list.finish_label_edit(true)?;
            if self.one_shot {
                self.closing.set(true);
            }
            return Ok(());
        }
        if let model::Details::Connection { interface, .. } = item.details {
            let result = self
                .background
                .run(move || network_config::rename_connection(&interface, &name))?
                .await;
            self.view.list.finish_label_edit(result.is_ok())?;
            if let Err(error) = result {
                self.report("重命名连接时出错", error).await;
            } else {
                self.refresh().await?;
                self.view.list.select(Some(row))?;
            }
            return Ok(());
        }
        let Some(path) = item.path else {
            return Ok(());
        };
        let result = self
            .background
            .run(move || fileops::Undo::rename(path, &name))?
            .await;
        match result {
            Ok((target, undo)) => {
                self.record_undo(undo);
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
                self.report("重命名文件或文件夹时出错", error).await;
                Ok(())
            }
        }
    }

    fn record_undo(&self, action: fileops::Undo) {
        let mut history = self.undo.borrow_mut();
        if history.len() == UNDO_LIMIT {
            history.remove(0);
        }
        history.push(action);
    }

    async fn undo(&self) -> Result<()> {
        let Some(mut action) = self.undo.borrow_mut().pop() else {
            return Ok(());
        };
        let home = self.home.clone();
        let (result, action) = self
            .background
            .run(move || {
                let result = action.reverse(&home);
                (result, action)
            })?
            .await;
        if result.is_err() && !action.exhausted() {
            self.record_undo(action);
        }
        self.refresh().await?;
        if let Some(path) = result? {
            self.select_path(&path)?;
        }
        Ok(())
    }

    async fn delete(&self, empty: bool, intent: fileops::DeleteIntent) -> Result<()> {
        // “我的电脑”等虚拟位置也有带路径的行，但这些行代表磁盘或连接。
        if !matches!(
            self.browser.borrow().location,
            Location::Directory(_) | Location::Desktop | Location::RecycleBin
        ) {
            return Ok(());
        }
        let paths = self.selected_paths()?;
        if paths.is_empty() && !empty {
            return Ok(());
        }
        let trash = self.availability()?.trash;
        let home = self.home.clone();
        let paths = if empty {
            Vec::new()
        } else {
            self.background
                .run(move || fileops::Deletion::prepare(&home, paths, intent))?
                .await?
        };
        let (title, message) = delete_prompt(
            &paths,
            trash || intent == fileops::DeleteIntent::Permanent,
            empty,
        );
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
        let (result, recycled, irreversible) = self
            .background
            .run(move || {
                if empty {
                    (fileops::empty_trash(&home).map(drop), Vec::new(), true)
                } else {
                    let mut recycled = Vec::new();
                    let mut irreversible = false;
                    let result = fileops::Deletion::commit(&home, paths, |entry| {
                        if let Some(entry) = entry {
                            recycled.push(entry);
                        } else {
                            irreversible = true;
                        }
                    });
                    (result, recycled, irreversible)
                }
            })?
            .await;
        if irreversible {
            self.undo.borrow_mut().clear();
        }
        if !recycled.is_empty() {
            self.record_undo(fileops::Undo::Delete(recycled));
        }
        self.refresh().await?;
        result?;
        Ok(())
    }
    /// 复制或移动并选中最后完成的目标；粘贴剪切项时只更新本次使用的选择。
    async fn transfer(
        &self,
        paths: Vec<PathBuf>,
        directory: PathBuf,
        cut: bool,
        revision: Option<u64>,
    ) -> Result<()> {
        self.view.status.set_text("正在复制或移动…\t\t")?;
        let home = self.home.clone();
        let fileops::Transfer {
            result,
            remaining,
            last,
        } = self
            .background
            .run(move || fileops::transfer(&home, paths, &directory, cut))?
            .await;
        let selection_update = if cut
            && let Some(revision) = revision
            && last.is_some()
        {
            self.offer_files(&remaining, true, Some(revision))
        } else {
            Ok(())
        };
        if last.is_some() {
            self.undo.borrow_mut().clear();
        }
        self.refresh().await?;
        if let Some(path) = last {
            self.select_path(&path)?;
        }
        result?;
        selection_update
    }
    fn file_selection(&self) -> liteos_ui::Result<Option<(&'static str, u64)>> {
        for mime in SELECTION_TYPES {
            if let Some(revision) = self.clipboard.revision(mime)? {
                return Ok(Some((mime, revision)));
            }
        }
        Ok(None)
    }

    fn offer_files(&self, paths: &[PathBuf], cut: bool, revision: Option<u64>) -> Result<()> {
        if paths.is_empty() {
            if revision.is_some() {
                self.clipboard.offer(&[], revision)?;
            }
            return Ok(());
        }
        let formats = file_uris::encode_selection(paths, cut, model::MAX_ENTRIES)?;
        self.clipboard.offer(
            &formats
                .each_ref()
                .map(|(mime, data)| (*mime, data.as_str())),
            revision,
        )?;
        Ok(())
    }

    async fn paste(&self) -> Result<()> {
        let directory = match &self.browser.borrow().location {
            Location::Directory(path) => path.clone(),
            Location::Desktop => self.home.join("Desktop"),
            _ => return Ok(()),
        };
        let Some((mime, revision)) = self.file_selection()? else {
            return Ok(());
        };
        let Some(payload) = self
            .clipboard
            .read(Some(mime), Duration::from_secs(2))?
            .await?
        else {
            return Ok(());
        };
        if self.clipboard.revision(mime)? != Some(revision) {
            return Ok(());
        }
        let (cut, paths) =
            file_uris::decode_selection(mime, payload.as_bytes(), model::MAX_ENTRIES)?;
        self.transfer(paths, directory, cut, Some(revision)).await
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
        self.transfer(paths, directory, cut, None).await
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
        self.undo.borrow_mut().clear();
        self.refresh().await?;
        if let Some(id) = self.select_path(&path)? {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            self.view.list.edit_label(id, &name)?;
        }
        Ok(())
    }
    async fn restore(&self, paths: Vec<PathBuf>) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.undo.borrow_mut().clear();
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

    async fn choose_columns(&self) -> Result<()> {
        let (location, initial) = {
            let browser = self.browser.borrow();
            (browser.location.clone(), browser.column_layout.clone())
        };
        let Some(layout) = crate::column_dialog::edit(
            &self.dialogs,
            &self.view.window,
            &location,
            initial.clone(),
        )
        .await?
        else {
            return Ok(());
        };
        if initial == layout {
            return Ok(());
        }
        let key = location.view_key()?;
        let saved = if layout == location.column_layout(&[]) {
            Vec::new()
        } else {
            layout.clone()
        };
        self.background
            .run(move || Settings::update(|settings| settings.set_folder_columns(key, saved)))?
            .await?;
        let mut browser = self.browser.borrow_mut();
        browser.set_column_layout(&layout);
        self.view.rows(&browser)?;
        Ok(())
    }
    fn toggle(&self, widget: &liteos_ui::Widget) -> liteos_ui::Result<()> {
        widget.set_state(widget.state()? ^ state::HIDDEN)?;
        self.view.fit_panel()
    }
    pub(super) async fn command(&self, command: Command) -> Result<()> {
        if let Some(style) = command.style() {
            self.view.list.set_list_style(style, view::MULTIPLE)?;
            let key = self.browser.borrow().location.view_key()?;
            let saved = match style {
                ListStyle::LargeIcons => FolderView::LargeIcons,
                ListStyle::SmallIcons => FolderView::SmallIcons,
                ListStyle::List => FolderView::List,
                ListStyle::Details => FolderView::Details,
            };
            self.background
                .run(move || Settings::update(|settings| settings.set_folder_view(key, saved)))?
                .await?;
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
                    if let Some(letter) = item.drive {
                        self.open_network_drive(letter).await?;
                    } else if let Some(target) = item.target {
                        self.navigate(target, Visit::Push, true).await?;
                    } else {
                        let path = item.path.clone();
                        match self.background.run(move || browser::open(item))?.await {
                            Ok(child) => self.record(child),
                            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                                if let Some(path) = path {
                                    self.open_with(path).await?;
                                } else {
                                    return Err(error.into());
                                }
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
            }
            Action::OpenWith => {
                if let Some(path) = self.selected()?.and_then(|item| item.path) {
                    self.open_with(path).await?;
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
                let location = self.browser.borrow().location.clone();
                if matches!(location, Location::Directory(_) | Location::Desktop) {
                    let paths = self.selected_paths()?;
                    self.offer_files(&paths, action == Action::Cut, None)?;
                }
            }
            Action::Paste => self.paste().await?,
            Action::ChooseColumns => self.choose_columns().await?,
            Action::CopyTo => self.copy_to(false).await?,
            Action::MoveTo => self.copy_to(true).await?,
            Action::SendToDocuments => {
                let paths = self.selected_paths()?;
                let home = self.home.clone();
                self.transfer(paths, home, false, None).await?;
            }
            Action::Delete => self.delete(false, fileops::DeleteIntent::Default).await?,
            Action::Undo => self.undo().await?,
            Action::EmptyTrash => self.delete(true, fileops::DeleteIntent::Default).await?,
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
                if let Some(row) = self.view.list.selection()? {
                    self.begin_rename(row)?;
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
            Action::MapDrive | Action::DisconnectDrive => {
                self.network_drive_dialog(action == Action::DisconnectDrive)
                    .await?
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
    pub(super) async fn execute(&self, job: Job) -> Result<()> {
        match job {
            Job::Command(command) => self.command(command).await,
            Job::DeletePermanently => self.delete(false, fileops::DeleteIntent::Permanent).await,
            Job::Go(location, visit, focus) => self.navigate(location, visit, focus).await,
            Job::Expand(id) => self.expand(id).await,
            Job::Rename(id, name) => self.rename(id, name).await,
            Job::Dropdown(action, at) => self.dropdown(action, at).await,
            Job::Context { on_item, at } => {
                let entries = chrome::context(&self.availability()?, on_item);
                let items: Vec<_> = entries.iter().map(chrome::Entry::item).collect();
                let menu = if let Some(at) = at {
                    self.dialogs.menu_at(&self.view.window, &items, at)?
                } else {
                    self.dialogs.menu(&self.view.window, &items)?
                };
                if let Some(command) = menu.await?.and_then(Command::from_id) {
                    self.command(command).await?;
                }
                Ok(())
            }
        }
    }
    pub(super) async fn report(&self, title: &str, error: impl std::fmt::Display) {
        let message = error.to_string();
        let _ = self.view.status.set_text(&format!("{message}\t\t"));
        if let Ok(dialog) =
            self.dialogs
                .message(&self.view.window, title, &message, MessageButtons::Ok)
        {
            let _ = dialog.await;
        }
    }
}
pub(super) fn delete_prompt(
    paths: &[fileops::Deletion],
    explicit_permanent: bool,
    empty: bool,
) -> (&'static str, String) {
    let permanent = paths
        .iter()
        .filter(|entry| entry.mode == fileops::DeleteMode::Permanent)
        .count();
    // shell32 67/68 与永久删除确认。
    let message = if empty {
        "确实要永久删除“回收站”中的所有项目吗?".to_owned()
    } else if explicit_permanent {
        if let [entry] = paths {
            let name = entry
                .path
                .file_name()
                .expect("prepared deletion has a name");
            format!("确实要永久删除“{}”吗?", name.to_string_lossy())
        } else {
            format!("确实要永久删除这 {} 项吗?", paths.len())
        }
    } else if permanent == paths.len() {
        format!(
            "这 {} 项无法移入本地回收站。确实要永久删除它们吗?",
            paths.len()
        )
    } else if permanent != 0 {
        format!(
            "其中 {permanent} 项无法移入本地回收站，将永久删除；其余项目将移入回收站。确实要删除这 {} 项吗?",
            paths.len()
        )
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
    (title, message)
}

/// 一个业务事务占用期，UI 仍处理输入和合成；关闭请求等已开始的文件副作用完成后异步兑现。
pub(super) struct Busy(pub(super) Rc<Session>);
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
