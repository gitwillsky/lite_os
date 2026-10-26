//! 通用文件对话框：共享控件持有交互，专属有界 worker 执行目录扫描与路径检查。
use super::{property_sheet::add, widgets::Tree};
use classic_ui::{Edit, EventKind};
use classic_ui_abi::{self as abi, event, state, widget};
use std::{
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};
const MAX_ENTRIES: usize = 4096;
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
    up: u64,
    list: u64,
    name: u64,
    primary: u64,
    cancel: u64,
    message: u64,
    filter: Option<Filter>,
    all_files: Option<u64>,
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
        let root = add(
            &mut tree,
            0,
            widget::COLUMN,
            "",
            abi::Layout {
                grow: 1,
                padding: 10,
                gap: 8,
                ..Default::default()
            },
        )?;
        let header = add(
            &mut tree,
            root,
            widget::ROW,
            "",
            abi::Layout {
                height: 26,
                gap: 8,
                ..Default::default()
            },
        )?;
        add(
            &mut tree,
            header,
            widget::LABEL,
            "查找范围：",
            abi::Layout {
                width: 80,
                ..Default::default()
            },
        )?;
        let location = add(
            &mut tree,
            header,
            widget::LABEL,
            "",
            abi::Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let up = add(
            &mut tree,
            header,
            widget::BUTTON,
            "上一级",
            abi::Layout {
                width: 80,
                ..Default::default()
            },
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
        let row = add(
            &mut tree,
            root,
            widget::ROW,
            "",
            abi::Layout {
                height: 26,
                gap: 8,
                ..Default::default()
            },
        )?;
        add(
            &mut tree,
            row,
            widget::LABEL,
            "文件名：",
            abi::Layout {
                width: 80,
                ..Default::default()
            },
        )?;
        let name = add(
            &mut tree,
            row,
            widget::EDIT,
            name,
            abi::Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let primary = add(
            &mut tree,
            row,
            widget::BUTTON,
            if mode == 0 { "打开" } else { "保存" },
            abi::Layout {
                width: 80,
                ..Default::default()
            },
        )?;
        tree.nodes.get_mut(&primary).expect("primary button").state = state::DEFAULT;
        let row = add(
            &mut tree,
            root,
            widget::ROW,
            "",
            abi::Layout {
                height: 26,
                gap: 8,
                ..Default::default()
            },
        )?;
        add(
            &mut tree,
            row,
            widget::LABEL,
            "文件类型：",
            abi::Layout {
                width: 80,
                ..Default::default()
            },
        )?;
        add(
            &mut tree,
            row,
            widget::LABEL,
            filter
                .as_ref()
                .map_or("所有文件", |filter| filter.label.as_str()),
            abi::Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        // 此勾选状态由 Tree 独占，切换时重新扫描并使旧序号结果失效。
        // 没有筛选的对话框不创建此控件，也不维护一份镜像布尔状态。
        let all_files = if filter
            .as_ref()
            .is_some_and(|filter| !filter.extensions.is_empty())
        {
            Some(add(
                &mut tree,
                row,
                widget::CHECKBOX,
                "所有文件",
                abi::Layout {
                    width: 100,
                    ..Default::default()
                },
            )?)
        } else {
            None
        };
        let cancel = add(
            &mut tree,
            row,
            widget::BUTTON,
            "取消",
            abi::Layout {
                width: 80,
                ..Default::default()
            },
        )?;
        let message = add(
            &mut tree,
            root,
            widget::LABEL,
            "",
            abi::Layout {
                height: 36,
                ..Default::default()
            },
        )?;
        tree.nodes.get_mut(&message).expect("message").state = state::WRAP;
        Ok(Self {
            tree,
            outcome: None,
            mode,
            directory,
            entries: Vec::new(),
            location,
            up,
            list,
            name,
            primary,
            cancel,
            message,
            filter,
            all_files,
            sequence: 0,
            busy: false,
            overwrite: None,
        })
    }
    fn status(&mut self, text: impl Into<String>) {
        self.tree.nodes.get_mut(&self.message).expect("status").text = text.into();
    }
    fn busy(&mut self, value: bool) {
        self.busy = value;
        for id in [self.up, self.list, self.name, self.primary] {
            let node = self.tree.nodes.get_mut(&id).expect("dialog control");
            if value {
                node.state |= state::DISABLED;
            } else {
                node.state &= !state::DISABLED;
            }
        }
        if let Some(id) = self.all_files {
            let node = self.tree.nodes.get_mut(&id).expect("file filter toggle");
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
        let all_files = self
            .all_files
            .is_some_and(|id| self.tree.nodes[&id].state & state::CHECKED != 0);
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
    pub fn apply(&mut self, completion: Completion) -> Action {
        if self.outcome.is_some() || completion.sequence != self.sequence {
            return Action::Pending;
        }
        self.busy(false);
        match completion.result {
            Err(error) => self.failed(error),
            Ok(Reply::Listing(directory, entries, truncated)) => {
                self.entries = entries;
                self.directory = directory;
                self.tree
                    .nodes
                    .get_mut(&self.location)
                    .expect("location")
                    .text = self.directory.display().to_string();
                let list = self.tree.nodes.get_mut(&self.list).expect("file list");
                list.rows = self
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        (
                            index as u64 + 1,
                            format!(
                                "{}{}",
                                if entry.directory { "▸ " } else { "" },
                                entry.name.to_string_lossy()
                            ),
                        )
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
            if Some(action.node) == self.all_files && action.kind == event::CHANGED {
                return self.list_request(window, self.directory.clone());
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
