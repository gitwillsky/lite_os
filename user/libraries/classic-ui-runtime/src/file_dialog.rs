//! 通用文件对话框：共享控件持有交互，专属有界 worker 执行目录扫描与路径检查。
use super::{outline::Outline, property_sheet::add, toolbar::Button, widgets::Tree};
use classic_ui::{Edit, EventKind, Resources};
use classic_ui_abi::{self as abi, event, state, widget};
use std::{
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};
const MAX_ENTRIES: usize = 4096;
/// comdlg32 的按钮与单行控件尺寸（75×23 逻辑像素）。
const BUTTON_WIDTH: i32 = 75;
const ROW_HEIGHT: i32 = 23;
/// “文件类型”下拉的第二项（所有文件）标识；第一项为调用方给出的筛选。
const ALL_FILES: u64 = 2;
/// `toolbar` 位图条中“向上一级”的帧序号（与 file-explorer 的“向上”相同）。
const UP_FRAME: i32 = 28;
pub struct Filter {
    label: String,
    extensions: Vec<String>,
}
impl Filter {
    pub fn new(label: String, extensions: Vec<String>) -> io::Result<Self> {
        if extensions.iter().any(|extension| {
            extension.is_empty()
                || extension.len() > 64
                || extension.starts_with('.')
                || !extension.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+')
                })
        }) {
            return Err(super::invalid(
                "file filter requires extensions without a leading dot",
            ));
        }
        Ok(Self { label, extensions })
    }
}
pub struct Entry {
    name: OsString,
    directory: bool,
}
pub enum Work {
    List(PathBuf, Vec<String>),
    Inspect(PathBuf),
}
pub struct Request {
    pub window: u64,
    sequence: u64,
    work: Work,
}
pub struct Completion {
    pub window: u64,
    sequence: u64,
    result: io::Result<Reply>,
}
enum Reply {
    Listing(PathBuf, Vec<Entry>, bool),
    Directory(PathBuf),
    File(PathBuf, bool),
}
pub enum Action {
    Pending,
    Request(Request),
    Done,
}
pub struct FileWorker {
    pub watch: u64,
    pub reader: UnixStream,
    sender: SyncSender<Request>,
    receiver: Receiver<Completion>,
}
impl FileWorker {
    pub fn new(watch: u64) -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        let (sender, jobs) = mpsc::sync_channel::<Request>(32);
        let (results, receiver) = mpsc::sync_channel(32);
        thread::Builder::new()
            .name("ui-file-dialog".into())
            .spawn(move || {
                // OWNER: 此线程独占同步文件 I/O，不持有 Tree、Wayland 或应用对象。
                // 两个有界 channel 限制待处理/已完成结果；Context 释放接收端后线程退出，无 UI join。
                let mut writer = writer;
                while let Ok(request) = jobs.recv() {
                    let result = match request.work {
                        Work::List(path, extensions) => list(path, &extensions),
                        Work::Inspect(path) => match fs::metadata(&path) {
                            Ok(meta) if meta.is_dir() => Ok(Reply::Directory(path)),
                            Ok(meta) if meta.is_file() => Ok(Reply::File(path, true)),
                            Ok(_) => Err(io::Error::other("请选择普通文件")),
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                Ok(Reply::File(path, false))
                            }
                            Err(error) => Err(error),
                        },
                    };
                    if results
                        .send(Completion {
                            window: request.window,
                            sequence: request.sequence,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                    loop {
                        match writer.write(&[1]) {
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            _ => break,
                        }
                    }
                }
            })?;
        Ok(Self {
            watch,
            reader,
            sender,
            receiver,
        })
    }
    pub fn submit(&self, request: Request) -> io::Result<()> {
        self.sender
            .try_send(request)
            .map_err(|error| io::Error::other(error.to_string()))
    }
    pub fn drain(&mut self) -> io::Result<Vec<Completion>> {
        let mut bytes = [0; 64];
        loop {
            match self.reader.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(self.receiver.try_iter().collect())
    }
}
fn list(path: PathBuf, extensions: &[String]) -> io::Result<Reply> {
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in fs::read_dir(&path)? {
        let entry = entry?;
        if entry.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }
        let kind = entry.file_type()?;
        let directory = kind.is_dir()
            || (kind.is_symlink() && fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir()));
        // 目录始终可导航；先筛选再计容量，避免无关文件耗尽显示额度。
        let name = entry.file_name();
        if !directory
            && !extensions.is_empty()
            && !extensions.iter().any(|extension| {
                let bytes = name.as_encoded_bytes();
                bytes.len() > extension.len()
                    && bytes[bytes.len() - extension.len() - 1] == b'.'
                    && bytes[bytes.len() - extension.len()..]
                        .eq_ignore_ascii_case(extension.as_bytes())
            })
        {
            continue;
        }
        if entries.len() == MAX_ENTRIES {
            truncated = true;
            break;
        }
        entries.push(Entry { name, directory });
    }
    entries.sort_by(|a, b| {
        b.directory
            .cmp(&a.directory)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(Reply::Listing(path, entries, truncated))
}
pub struct FileDialog {
    pub tree: Tree,
    pub outcome: Option<Option<PathBuf>>,
    mode: u32,
    directory: PathBuf,
    entries: Vec<Entry>,
    location: u64,
    /// 位置下拉的祖先链（从根到当前目录），下拉项标识为序号 + 1；缺失则无法由选择项换算目标目录。
    places: Vec<PathBuf>,
    up: u64,
    /// 左侧位置栏（桌面/我的文档/我的电脑）；标识为序号 + 1，缺失则无法由选择项换算目标目录。
    bar: u64,
    shortcuts: Vec<PathBuf>,
    list: u64,
    name: u64,
    primary: u64,
    cancel: u64,
    message: u64,
    filter: Option<Filter>,
    types: u64,
    sequence: u64,
    busy: bool,
    overwrite: Option<PathBuf>,
}
impl FileDialog {
    pub fn new(
        mode: u32,
        directory: PathBuf,
        name: &str,
        filter: Option<Filter>,
    ) -> io::Result<Self> {
        let mut tree = Tree::new();
        let frame = add(
            &mut tree,
            0,
            widget::COLUMN,
            "",
            abi::Layout {
                grow: 1,
                padding: 8,
                gap: 6,
                ..Default::default()
            },
        )?;
        let main = add(
            &mut tree,
            frame,
            widget::ROW,
            "",
            abi::Layout {
                grow: 1,
                gap: 6,
                ..Default::default()
            },
        )?;
        let bar = add(
            &mut tree,
            main,
            widget::LIST,
            "",
            abi::Layout {
                width: 84,
                ..Default::default()
            },
        )?;
        {
            let node = tree.nodes.get_mut(&bar).expect("places bar");
            node.state |= state::CLIENT_EDGE;
            node.table.as_mut().expect("list model").mode = 1;
        }
        let root = add(
            &mut tree,
            main,
            widget::COLUMN,
            "",
            abi::Layout {
                grow: 1,
                gap: 6,
                ..Default::default()
            },
        )?;
        let row = |tree: &mut Tree| {
            add(
                tree,
                root,
                widget::ROW,
                "",
                abi::Layout {
                    height: ROW_HEIGHT,
                    gap: 6,
                    ..Default::default()
                },
            )
        };
        let label = |tree: &mut Tree, row: u64, text: &str| {
            add(
                tree,
                row,
                widget::LABEL,
                text,
                abi::Layout {
                    width: 84,
                    ..Default::default()
                },
            )
        };
        let button = |tree: &mut Tree, row: u64, text: &str| {
            add(
                tree,
                row,
                widget::BUTTON,
                text,
                abi::Layout {
                    width: BUTTON_WIDTH,
                    ..Default::default()
                },
            )
        };
        let header = row(&mut tree)?;
        label(&mut tree, header, "查找范围(I):")?;
        let location = add(
            &mut tree,
            header,
            widget::COMBO,
            "",
            abi::Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        tree.nodes.get_mut(&location).expect("location").state |= state::DROPDOWN_LIST;
        let up = add(
            &mut tree,
            header,
            widget::TOOLBAR,
            "",
            abi::Layout::default(),
        )?;
        let list = add(
            &mut tree,
            root,
            widget::LIST,
            "",
            abi::Layout {
                grow: 1,
                min_height: 80,
                ..Default::default()
            },
        )?;
        {
            // Win2000 打开对话框默认“列表”视图（图标 + 名称纵向排列、按列折行）。
            let node = tree.nodes.get_mut(&list).expect("file list");
            node.state |= state::CLIENT_EDGE;
            node.table.as_mut().expect("list model").mode = 3;
        }
        let row_name = row(&mut tree)?;
        label(&mut tree, row_name, "文件名(N):")?;
        let name = add(
            &mut tree,
            row_name,
            widget::EDIT,
            name,
            abi::Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let primary = button(
            &mut tree,
            row_name,
            if mode == 0 { "打开(O)" } else { "保存(S)" },
        )?;
        tree.nodes.get_mut(&primary).expect("primary button").state = state::DEFAULT;
        let row_type = row(&mut tree)?;
        label(&mut tree, row_type, "文件类型(T):")?;
        let types = add(
            &mut tree,
            row_type,
            widget::COMBO,
            "",
            abi::Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        tree.nodes.get_mut(&types).expect("file type").state |= state::DROPDOWN_LIST;
        // 类型下拉由 Tree 独占选择状态：第 1 项为调用方筛选，第 2 项为所有文件；
        // 没有扩展名筛选时只有“所有文件”一项，不维护镜像布尔值。
        let cancel = button(&mut tree, row_type, "取消")?;
        let message = add(
            &mut tree,
            root,
            widget::LABEL,
            "",
            abi::Layout {
                height: 32,
                ..Default::default()
            },
        )?;
        tree.nodes.get_mut(&message).expect("message").state = state::WRAP;
        {
            let filtered = filter
                .as_ref()
                .is_some_and(|filter| !filter.extensions.is_empty());
            let label = filter
                .as_ref()
                .filter(|_| filtered)
                .map_or("所有文件 (*.*)", |filter| filter.label.as_str());
            let node = tree.nodes.get_mut(&types).expect("file type");
            node.rows = if filtered {
                vec![(1, label.to_owned()), (ALL_FILES, "所有文件 (*.*)".into())]
            } else {
                vec![(1, label.to_owned())]
            };
            node.outline = Some(Outline::list(
                &node.rows.iter().map(|row| row.0).collect::<Vec<_>>(),
                false,
            ));
            node.selected = Some(1);
            node.edit = Some(Edit::new(label));
        }
        Ok(Self {
            tree,
            outcome: None,
            mode,
            directory,
            entries: Vec::new(),
            location,
            places: Vec::new(),
            up,
            bar,
            shortcuts: Vec::new(),
            list,
            name,
            primary,
            cancel,
            message,
            filter,
            types,
            sequence: 0,
            busy: false,
            overwrite: None,
        })
    }
    /// 装配依赖系统素材的部分：向上工具栏按钮与左侧位置栏；窗口创建后调用一次。
    pub fn decorate(&mut self, resources: &mut Resources) {
        let strips = ["default", "hot", "disabled"]
            .map(|variant| resources.bitmap(&format!("toolbar-{variant}-1x")));
        self.tree
            .nodes
            .get_mut(&self.up)
            .expect("up toolbar")
            .toolbar
            .get_or_insert_with(Default::default)
            .set_buttons(vec![Button::new(
                1,
                strips,
                UP_FRAME,
                "上一级".into(),
                0,
                0,
            )]);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut places = Vec::new();
        if let Some(home) = &home {
            places.push(("桌面", "desktop", home.join("Desktop")));
            places.push(("我的文档", "my-documents", home.clone()));
        }
        places.push(("我的电脑", "my-computer", PathBuf::from("/")));
        let node = self.tree.nodes.get_mut(&self.bar).expect("places bar");
        node.rows = places
            .iter()
            .enumerate()
            .map(|(index, place)| (index as u64 + 1, place.0.to_owned()))
            .collect();
        node.row_icons = places
            .iter()
            .enumerate()
            .map(|(index, place)| {
                (
                    index as u64 + 1,
                    (resources.icon(place.1, 16), resources.icon(place.1, 32)),
                )
            })
            .collect();
        self.shortcuts = places.into_iter().map(|place| place.2).collect();
    }
    /// 位置下拉显示从根到当前目录的祖先链并选中当前目录（Win2000 “查找范围”）。
    fn show_location(&mut self, resources: &mut Resources) {
        self.places = self.directory.ancestors().map(PathBuf::from).collect();
        self.places.reverse();
        let rows: Vec<(u64, String)> = self
            .places
            .iter()
            .enumerate()
            .map(|(index, place)| {
                let name = place.file_name().map_or_else(
                    || place.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                );
                (index as u64 + 1, name)
            })
            .collect();
        let ids: Vec<u64> = rows.iter().map(|row| row.0).collect();
        let current = ids.last().copied();
        let icon: Option<Rc<classic_ui::Bitmap>> = resources.icon("folder-closed", 16);
        let node = self.tree.nodes.get_mut(&self.location).expect("location");
        node.row_icons = ids.iter().map(|id| (*id, (icon.clone(), None))).collect();
        node.edit = Some(Edit::new(rows.last().map_or("", |row| row.1.as_str())));
        node.rows = rows;
        node.outline = Some(Outline::list(&ids, true));
        node.selected = current;
    }
    fn status(&mut self, text: impl Into<String>) {
        self.tree.nodes.get_mut(&self.message).expect("status").text = text.into();
    }
    fn busy(&mut self, value: bool) {
        self.busy = value;
        for id in [
            self.up,
            self.bar,
            self.list,
            self.name,
            self.primary,
            self.location,
            self.types,
        ] {
            let node = self.tree.nodes.get_mut(&id).expect("dialog control");
            if value {
                node.state |= state::DISABLED;
            } else {
                node.state &= !state::DISABLED;
            }
        }
        self.tree.layout_dirty = true;
    }
    pub fn request(&mut self, window: u64, work: Work) -> Action {
        self.sequence += 1;
        self.busy(true);
        self.status("正在读取…");
        Action::Request(Request {
            window,
            sequence: self.sequence,
            work,
        })
    }
    pub fn initial(&mut self, window: u64) -> Action {
        self.list_request(window, self.directory.clone())
    }
    fn list_request(&mut self, window: u64, path: PathBuf) -> Action {
        let all_files = self.tree.nodes[&self.types].selected == Some(ALL_FILES);
        let extensions = if all_files {
            Vec::new()
        } else {
            self.filter
                .as_ref()
                .map_or_else(Vec::new, |filter| filter.extensions.clone())
        };
        self.request(window, Work::List(path, extensions))
    }
    pub fn failed(&mut self, error: io::Error) {
        self.busy(false);
        self.status(error.to_string());
    }
    pub fn apply(&mut self, completion: Completion, resources: &mut Resources) -> Action {
        if self.outcome.is_some() || completion.sequence != self.sequence {
            return Action::Pending;
        }
        self.busy(false);
        match completion.result {
            Err(error) => self.failed(error),
            Ok(Reply::Listing(directory, entries, truncated)) => {
                self.entries = entries;
                self.directory = directory;
                self.show_location(resources);
                let folder = (
                    resources.icon("folder-closed", 16),
                    resources.icon("folder-closed", 32),
                );
                let file = (
                    resources.icon("file-generic", 16),
                    resources.icon("file-generic", 32),
                );
                let list = self.tree.nodes.get_mut(&self.list).expect("file list");
                list.rows = self
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| (index as u64 + 1, entry.name.to_string_lossy().into()))
                    .collect();
                list.row_icons = self
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let (small, large) = if entry.directory { &folder } else { &file };
                        (index as u64 + 1, (small.clone(), large.clone()))
                    })
                    .collect();
                list.selected = None;
                list.position.position = 0;
                self.overwrite = None;
                self.status(if truncated {
                    "目录过大，仅显示前 4096 项"
                } else {
                    ""
                });
            }
            Ok(Reply::Directory(path)) => {
                *self
                    .tree
                    .nodes
                    .get_mut(&self.name)
                    .expect("name")
                    .edit
                    .as_mut()
                    .expect("edit") = Edit::new("");
                return self.list_request(completion.window, path);
            }
            Ok(Reply::File(path, exists)) => {
                if self.mode == 0 && !exists {
                    self.status("找不到文件，请检查文件名是否正确。");
                } else if self.mode == 1 && exists && self.overwrite.as_ref() != Some(&path) {
                    self.status("文件已存在，再次点击“保存”以替换它。");
                    self.overwrite = Some(path);
                } else {
                    self.outcome = Some(Some(path));
                    return Action::Done;
                }
            }
        }
        Action::Pending
    }
    pub fn actions(
        &mut self,
        window: u64,
        actions: Vec<abi::Event>,
        input: &EventKind,
        close: bool,
    ) -> Action {
        if self.outcome.is_some() {
            return Action::Pending;
        }
        if close
            || matches!(input, EventKind::Key(key) if key.pressed && key.code == 1)
            || actions
                .iter()
                .any(|action| action.node == self.cancel && action.kind == event::ACTIVATE)
        {
            self.outcome = Some(None);
            return Action::Done;
        }
        if self.busy {
            return Action::Pending;
        }
        for action in actions {
            if action.node == self.bar
                && action.kind == event::SELECT
                && let Some(place) = action
                    .value
                    .checked_sub(1)
                    .and_then(|index| self.shortcuts.get(index as usize))
            {
                let place = place.clone();
                return self.list_request(window, place);
            }
            if action.node == self.types && action.kind == event::SELECT {
                return self.list_request(window, self.directory.clone());
            }
            if action.node == self.location
                && action.kind == event::SELECT
                && let Some(place) = action
                    .value
                    .checked_sub(1)
                    .and_then(|index| self.places.get(index as usize))
                && *place != self.directory
            {
                let place = place.clone();
                return self.list_request(window, place);
            }
            if action.kind == event::CHANGED && action.node == self.name {
                self.overwrite = None;
                self.status("");
            }
            if action.node == self.list
                && matches!(action.kind, event::SELECT | event::ACTIVATE)
                && let Some(entry) = action
                    .value
                    .checked_sub(1)
                    .and_then(|index| self.entries.get(index as usize))
            {
                if !entry.directory {
                    *self
                        .tree
                        .nodes
                        .get_mut(&self.name)
                        .expect("name")
                        .edit
                        .as_mut()
                        .expect("edit") = Edit::new(&entry.name.to_string_lossy());
                    self.overwrite = None;
                    self.status("");
                }
                if action.kind == event::ACTIVATE {
                    let entry = &self.entries[action.value as usize - 1];
                    return self.request(window, Work::Inspect(self.directory.join(&entry.name)));
                }
            }
            if action.kind == event::ACTIVATE
                && action.node == self.up
                && let Some(parent) = self.directory.parent()
            {
                return self.list_request(window, parent.to_owned());
            }
            if action.kind == event::ACTIVATE && action.node == self.primary {
                let name = self.tree.nodes[&self.name].text().trim();
                if name.is_empty() {
                    return Action::Pending;
                }
                let selected = self.tree.nodes[&self.list]
                    .selected
                    .and_then(|id| self.entries.get(id as usize - 1));
                let path = if let Some(entry) =
                    selected.filter(|entry| entry.name.to_string_lossy() == name)
                {
                    self.directory.join(&entry.name)
                } else {
                    self.directory.join(name)
                };
                return self.request(window, Work::Inspect(path));
            }
        }
        Action::Pending
    }
}
