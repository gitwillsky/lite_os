//! 文件查找：不可变查询、可恢复的分批遍历，以及 UI 线程的开始/停止命令。
//! Cursor 在后台任务间移交，既不持有控件，也不复制 Browser 已发布的结果。
mod filters;
use crate::{
    model::{self, Icon, Item, Location},
    session::Session,
};
use glob::Pattern;
use liteos_ui::{Layout, Widget, WidgetKind as Kind, WidgetSpec, state};
use std::{
    cell::RefCell,
    collections::VecDeque,
    fs::{self, File, ReadDir},
    future::{Future, poll_fn},
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

/// 一次已经提交的搜索；表单草稿不是当前查询，只有单击“立即搜索”才发表。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Query {
    pub name: String,
    pub text: String,
    pub location: String,
    pub recursive: bool,
    /// 筛选快照独立分配，避免把每个普通文件项携带的 Location 扩大到整份表单大小。
    pub filters: Box<filters::Draft>,
}

/// 单接收者的 UI 命令邮箱；新开始/停止取代尚未消费的命令，不排队积累过时查询。
/// Waker 只唤醒现有窗口任务；没有新线程、轮询定时器或跨线程 UI 访问。
#[derive(Default)]
pub struct Control {
    request: RefCell<Option<Option<Query>>>,
    wake: RefCell<Option<Waker>>,
}
impl Control {
    pub fn request(&self, query: Option<Query>) {
        self.request.replace(Some(query));
        self.notify();
    }
    pub fn notify(&self) {
        if let Some(wake) = self.wake.borrow_mut().take() {
            wake.wake();
        }
    }
    fn poll(&self, cx: &mut Context<'_>) -> Poll<Option<Query>> {
        if let Some(request) = self.request.borrow_mut().take() {
            return Poll::Ready(request);
        }
        self.wake.replace(Some(cx.waker().clone()));
        Poll::Pending
    }
}

pub struct Pane {
    pub root: Widget,
    pub name: Widget,
    pub text: Widget,
    pub location: Widget,
    pub recursive: Widget,
    pub start: Widget,
    pub stop: Widget,
    pub browse: Widget,
    pub options: filters::Controls,
    _labels: Vec<Widget>,
}
fn child(parent: &Widget, kind: Kind, text: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}
impl Pane {
    pub fn new(parent: &Widget, location: &str) -> liteos_ui::Result<Self> {
        let root = child(
            parent,
            Kind::Column,
            "",
            Layout {
                grow: 1,
                padding: 8,
                gap: 4,
                ..Default::default()
            },
        )?;
        root.set_state(state::VSCROLL)?;
        let mut labels = Vec::new();
        labels.push(child(
            &root,
            Kind::Label,
            "搜索文件或文件夹",
            Layout::default(),
        )?);
        let mut edit = |label: &str, kind, text: &str| -> liteos_ui::Result<Widget> {
            let heading = child(&root, Kind::Label, label, Layout::default())?;
            heading.set_state(state::WRAP)?;
            labels.push(heading);
            child(&root, kind, text, Layout::default())
        };
        let name = edit("要搜索的文件或文件夹名(N):", Kind::Edit, "")?;
        let text = edit("包含文字(C):", Kind::Edit, "")?;
        let location = edit("搜索范围(L):", Kind::Combo, location)?;
        location.set_tree_rows(&[
            liteos_ui::TreeRow {
                id: 1,
                parent: 0,
                text: "我的电脑",
                expanded: false,
                has_children: false,
            },
            liteos_ui::TreeRow {
                id: 2,
                parent: 0,
                text: "我的文档",
                expanded: false,
                has_children: false,
            },
        ])?;
        let browse = child(&root, Kind::Button, "浏览(B)...", Layout::default())?;
        let buttons = child(
            &root,
            Kind::Row,
            "",
            Layout {
                gap: 6,
                ..Default::default()
            },
        )?;
        let start = child(
            &buttons,
            Kind::Button,
            "立即搜索(E)",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let stop = child(
            &buttons,
            Kind::Button,
            "停止(T)",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        labels.push(buttons);
        stop.set_state(state::DISABLED)?;
        let options = filters::Controls::new(&root)?;
        let recursive = options.recursive.clone();
        Ok(Self {
            root,
            name,
            text,
            location,
            recursive,
            start,
            stop,
            browse,
            options,
            _labels: labels,
        })
    }
    pub fn query(&self) -> liteos_ui::Result<Query> {
        Ok(Query {
            name: self.name.text()?,
            text: self.text.text()?,
            location: self.location.text()?,
            recursive: self.recursive.state()? & state::CHECKED != 0,
            filters: Box::new(self.options.draft()?),
        })
    }
    pub fn restore(&self, query: &Query) -> liteos_ui::Result<()> {
        self.name.set_text(&query.name)?;
        self.text.set_text(&query.text)?;
        self.location.set_text(&query.location)?;
        self.options.restore(&query.filters)?;
        self.recursive
            .set_state(if query.recursive { state::CHECKED } else { 0 })
    }
    pub fn running(&self, running: bool) -> liteos_ui::Result<()> {
        self.stop
            .set_state(if running { 0 } else { state::DISABLED })
    }
    pub fn editing(&self, node: u64) -> bool {
        [self.name.id(), self.text.id(), self.location.id()].contains(&node)
            || self.options.editing(node)
    }
}

struct Directory {
    entries: ReadDir,
    identity: (u64, u64),
}
/// KMP 前缀表在查询级构建；扫描块之间保留匹配位置，不保存文件全文。
struct Needle {
    chars: Vec<char>,
    prefix: Vec<usize>,
    sensitive: bool,
}
impl Needle {
    fn new(text: &str, sensitive: bool) -> Self {
        let chars: Vec<_> = if sensitive {
            text.chars().collect()
        } else {
            text.chars().flat_map(char::to_lowercase).collect()
        };
        let mut prefix = vec![0; chars.len()];
        let mut matched = 0;
        for index in 1..chars.len() {
            while matched > 0 && chars[index] != chars[matched] {
                matched = prefix[matched - 1];
            }
            if chars[index] == chars[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }
        Self {
            chars,
            prefix,
            sensitive,
        }
    }
    fn feed(&self, matched: &mut usize, ch: char) -> bool {
        if self.chars.is_empty() {
            return true;
        }
        let mut consume = |ch| {
            while *matched > 0 && self.chars[*matched] != ch {
                *matched = self.prefix[*matched - 1];
            }
            if self.chars[*matched] == ch {
                *matched += 1;
            }
            *matched == self.chars.len()
        };
        if self.sensitive {
            consume(ch)
        } else {
            ch.to_lowercase().any(consume)
        }
    }
}
#[derive(Clone, Copy)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}
struct Content {
    file: File,
    path: PathBuf,
    matched: usize,
    encoding: Encoding,
    carry: Vec<u8>,
    surrogate: Option<u16>,
}
impl Content {
    fn open(path: PathBuf) -> io::Result<Self> {
        // 文件可能在目录枚举后被替换：不跟随新链接，不阻塞打开 FIFO，fstat 再拒绝非普通文件。
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        if !file.metadata()?.is_file() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let count = file.metadata()?.len().min(3) as usize;
        let mut carry = vec![0; count];
        file.read_exact(&mut carry)?;
        let encoding = if carry.starts_with(&[0xFF, 0xFE]) {
            carry.drain(..2);
            Encoding::Utf16Le
        } else if carry.starts_with(&[0xFE, 0xFF]) {
            carry.drain(..2);
            Encoding::Utf16Be
        } else {
            if carry.starts_with(&[0xEF, 0xBB, 0xBF]) {
                carry.clear();
            }
            Encoding::Utf8
        };
        Ok(Self {
            file,
            path,
            matched: 0,
            encoding,
            carry,
            surrogate: None,
        })
    }
    fn block(&mut self, needle: &Needle) -> io::Result<(bool, bool)> {
        // 固定块上限使取消不依赖文件大小；尾部只保留未完成的一个编码单元。
        let mut data = std::mem::take(&mut self.carry);
        let prefix = data.len();
        data.resize(prefix + 64 * 1024, 0);
        let n = loop {
            match self.file.read(&mut data[prefix..]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                value => break value?,
            }
        };
        data.truncate(prefix + n);
        let mut tail = data.len();
        match self.encoding {
            Encoding::Utf8 => {
                let mut offset = 0;
                while offset < data.len() {
                    let (text, skip) = match std::str::from_utf8(&data[offset..]) {
                        Ok(text) => (text, 0),
                        Err(error) => {
                            let end = offset + error.valid_up_to();
                            let text = std::str::from_utf8(&data[offset..end])
                                .expect("validated UTF-8 prefix");
                            (text, error.error_len().unwrap_or(0))
                        }
                    };
                    for ch in text.chars() {
                        if needle.feed(&mut self.matched, ch) {
                            return Ok((true, true));
                        }
                    }
                    offset += text.len();
                    if offset == data.len() {
                        break;
                    }
                    if skip == 0 {
                        if n != 0 {
                            tail = offset;
                        }
                        break;
                    }
                    if needle.feed(&mut self.matched, '\u{FFFD}') {
                        return Ok((true, true));
                    }
                    offset += skip;
                }
            }
            Encoding::Utf16Le | Encoding::Utf16Be => {
                for pair in data.as_chunks::<2>().0 {
                    let code = match self.encoding {
                        Encoding::Utf16Le => u16::from_le_bytes(*pair),
                        _ => u16::from_be_bytes(*pair),
                    };
                    if let Some(high) = self.surrogate.take() {
                        if (0xDC00..=0xDFFF).contains(&code) {
                            let ch = char::from_u32(
                                0x10000
                                    + ((u32::from(high) - 0xD800) << 10)
                                    + (u32::from(code) - 0xDC00),
                            )
                            .expect("UTF-16 surrogate pair");
                            if needle.feed(&mut self.matched, ch) {
                                return Ok((true, true));
                            }
                            continue;
                        }
                        if needle.feed(&mut self.matched, '\u{FFFD}') {
                            return Ok((true, true));
                        }
                    }
                    if (0xD800..=0xDBFF).contains(&code) {
                        self.surrogate = Some(code);
                    } else if needle.feed(
                        &mut self.matched,
                        char::from_u32(u32::from(code)).unwrap_or('\u{FFFD}'),
                    ) {
                        return Ok((true, true));
                    }
                }
                if !data.len().is_multiple_of(2) && n != 0 {
                    tail = data.len() - 1;
                }
            }
        }
        data.copy_within(tail.., 0);
        data.truncate(data.len() - tail);
        self.carry = data;
        Ok((false, n == 0))
    }
}
struct Cursor {
    query: Query,
    patterns: Vec<Pattern>,
    needle: Needle,
    filters: filters::Compiled,
    roots: VecDeque<PathBuf>,
    directories: Vec<Directory>,
    device: u64,
    content: Option<Content>,
    associations: desktop_entry::Associations,
    scanned: usize,
    found: usize,
    skipped: usize,
}
struct Batch {
    cursor: Cursor,
    items: Vec<Item>,
}
impl Cursor {
    fn new(query: Query, home: PathBuf) -> io::Result<Self> {
        let roots: Vec<PathBuf> = match query.location.trim() {
            "我的电脑" => Location::MyComputer
                .list(&fs::read_to_string("/proc/mounts")?)?
                .0
                .into_iter()
                .filter(|item| matches!(item.icon, Icon::Disk | Icon::OpticalDisc))
                .filter_map(|item| item.path)
                .collect(),
            "我的文档" => vec![home],
            "" => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "请选择搜索范围。",
                ));
            }
            path => {
                let path = PathBuf::from(path);
                vec![if path.is_absolute() {
                    path
                } else {
                    home.join(path)
                }]
            }
        };
        let mut canonical = Vec::new();
        for path in roots {
            let path = fs::canonicalize(path)?;
            // 遍历不跨设备；父目录不能代替其下另一个挂载卷的独立根。
            if !canonical.contains(&path) {
                canonical.push(path);
            }
        }
        let mut patterns = Vec::new();
        for pattern in query
            .name
            .split(';')
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let pattern = if pattern == "*.*" { "*" } else { pattern };
            let mut escaped = String::new();
            // Windows 的文件名语法只有 * / ?；glob 的字符类括号在此是字面字符。
            let pattern_text = if query.filters.case_sensitive {
                pattern.to_owned()
            } else {
                pattern.to_lowercase()
            };
            for ch in pattern_text.chars() {
                match ch {
                    '[' => escaped.push_str("[[]"),
                    ']' => escaped.push_str("[]]"),
                    _ => escaped.push(ch),
                }
            }
            if !pattern.contains(['*', '?']) {
                escaped = format!("*{escaped}*");
            }
            patterns.push(
                Pattern::new(&escaped)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
            );
        }
        let needle = Needle::new(&query.text, query.filters.case_sensitive);
        let associations = desktop_entry::Associations::load();
        let filters = query.filters.compile(&associations)?;
        Ok(Self {
            query,
            patterns,
            needle,
            filters,
            roots: canonical.into(),
            directories: Vec::new(),
            device: 0,
            content: None,
            associations,
            scanned: 0,
            found: 0,
            skipped: 0,
        })
    }
    fn done(&self) -> bool {
        self.roots.is_empty() && self.directories.is_empty() && self.content.is_none()
    }
    fn append(&mut self, path: &Path, items: &mut Vec<Item>) {
        self.found += 1;
        if self.found > model::MAX_ENTRIES {
            self.roots.clear();
            self.directories.clear();
            self.content = None;
            return;
        }
        items.push(model::describe(path, path, &self.associations));
    }
    fn step(mut self) -> io::Result<Batch> {
        let mut items = Vec::new();
        let mut read_blocks = 0;
        for _ in 0..256 {
            if let Some(mut content) = self.content.take() {
                read_blocks += 1;
                match content.block(&self.needle) {
                    Ok((true, _)) => self.append(&content.path, &mut items),
                    Ok((false, false)) => {
                        self.content = Some(content);
                    }
                    Ok((false, true)) => {}
                    Err(_) => self.skipped += 1,
                }
                if read_blocks == 16 {
                    break;
                }
                continue;
            }
            let entry = if let Some(directory) = self.directories.last_mut() {
                match directory.entries.next() {
                    Some(Ok(entry)) => entry,
                    Some(Err(_)) => {
                        self.skipped += 1;
                        continue;
                    }
                    None => {
                        self.directories.pop();
                        continue;
                    }
                }
            } else if let Some(root) = self.roots.pop_front() {
                let metadata = fs::metadata(&root)?;
                self.device = metadata.dev();
                self.directories.push(Directory {
                    entries: fs::read_dir(root)?,
                    identity: (metadata.dev(), metadata.ino()),
                });
                continue;
            } else {
                break;
            };
            self.scanned += 1;
            let path = entry.path();
            let metadata = match entry.metadata() {
                Ok(value) => value,
                Err(_) => {
                    self.skipped += 1;
                    continue;
                }
            };
            if metadata.is_dir() && self.query.recursive && metadata.dev() == self.device {
                let identity = (metadata.dev(), metadata.ino());
                if !self.directories.iter().any(|dir| dir.identity == identity) {
                    match fs::read_dir(&path) {
                        Ok(entries) => self.directories.push(Directory { entries, identity }),
                        Err(_) => self.skipped += 1,
                    }
                }
            }
            if !self.filters.accepts(&path, &metadata, &self.associations) {
                continue;
            }
            let raw_name = entry.file_name();
            let name = if self.query.filters.case_sensitive {
                raw_name.to_string_lossy().into_owned()
            } else {
                raw_name.to_string_lossy().to_lowercase()
            };
            if !self.patterns.is_empty()
                && !self.patterns.iter().any(|pattern| pattern.matches(&name))
            {
                continue;
            }
            if self.needle.chars.is_empty() {
                self.append(&path, &mut items);
            } else if metadata.is_file() {
                match Content::open(path) {
                    Ok(content) => self.content = Some(content),
                    Err(_) => self.skipped += 1,
                }
            }
            if self.found > model::MAX_ENTRIES {
                break;
            }
        }
        Ok(Batch {
            cursor: self,
            items,
        })
    }
}

impl Session {
    pub(super) async fn search_types(&self) -> crate::Result<()> {
        let types = self
            .background
            .run(|| {
                let associations = desktop_entry::Associations::load();
                let mut types: Vec<_> = associations
                    .registered_types()
                    .map(|(_, label)| label.to_owned())
                    .collect();
                types.sort();
                types.dedup();
                types
            })?
            .await;
        if let Some(pane) = self.view.search.borrow().as_ref() {
            pane.options.set_types(&types)?;
        }
        Ok(())
    }
    /// 打开搜索浏览栏；查询草稿以当前目录为范围，不立即扫描磁盘。
    pub(super) fn show_search(&self) -> liteos_ui::Result<()> {
        let location = match &self.browser.borrow().location {
            Location::Directory(path) => path.to_string_lossy().into_owned(),
            Location::Desktop => self.home.join("Desktop").to_string_lossy().into_owned(),
            _ => "我的电脑".into(),
        };
        self.view.show_search(&location, None)?;
        self.view.fit_panel(&self.browser.borrow())
    }
    pub(super) async fn search_browse(&self) -> crate::Result<()> {
        let value = self
            .view
            .search
            .borrow()
            .as_ref()
            .map(|pane| pane.location.text())
            .transpose()?
            .unwrap_or_default();
        let start = match value.as_str() {
            "我的文档" => self.home.clone(),
            "我的电脑" | "" => PathBuf::from("/"),
            _ => PathBuf::from(value),
        };
        if let Some(path) = crate::folder_dialog::browse(
            &self.dialogs,
            &self.background,
            &self.view.window,
            "浏览文件夹",
            "选择要搜索的文件夹:",
            &start,
        )
        .await?
            && let Some(pane) = self.view.search.borrow().as_ref()
        {
            pane.location.set_text(&path.to_string_lossy())?;
        }
        Ok(())
    }
}

/// 一个窗口只运行此接收者；命令优先于后台结果，停止后不会把迟到批次发表到新位置。
/// 正在进行的后台批次可在取消后自行释放 Cursor，不在 UI join；工作池仍由 SDK 统一限流。
pub async fn run(session: Rc<Session>) -> crate::Result<()> {
    let control = &session.search;
    let mut command = poll_fn(|cx| control.poll(cx)).await;
    'commands: loop {
        let Some(query) = command else {
            if let Some(pane) = session.view.search.borrow().as_ref() {
                pane.running(false)?;
            }
            if matches!(session.browser.borrow().location, Location::Search(Some(_))) {
                session.view.status.set_text(&format!(
                    "搜索已停止。找到 {} 个对象。\t\t",
                    session.browser.borrow().items.len()
                ))?;
            }
            command = poll_fn(|cx| control.poll(cx)).await;
            continue;
        };
        if let Some(pane) = session.view.search.borrow().as_ref() {
            pane.running(true)?;
        }
        let mut progress = std::time::Instant::now();
        let mut cursor: Option<Cursor> = None;
        loop {
            let work = if let Some(cursor) = cursor.take() {
                session.background.run(move || cursor.step())
            } else {
                let home = session.home.clone();
                let initial = query.clone();
                session
                    .background
                    .run(move || Cursor::new(initial, home).and_then(Cursor::step))
            };
            let mut work = match work {
                Ok(work) => work,
                Err(error) => {
                    session
                        .view
                        .status
                        .set_text(&format!("无法开始搜索：{error}\t\t"))?;
                    if let Some(pane) = session.view.search.borrow().as_ref() {
                        pane.running(false)?;
                    }
                    command = poll_fn(|cx| control.poll(cx)).await;
                    continue 'commands;
                }
            };
            let batch = poll_fn(|cx| {
                if let Poll::Ready(command) = control.poll(cx) {
                    return Poll::Ready(Err(command));
                }
                Pin::new(&mut work).poll(cx).map(Ok)
            })
            .await;
            let batch = match batch {
                Err(next) => {
                    command = next;
                    continue 'commands;
                }
                Ok(Err(error)) => {
                    session
                        .view
                        .status
                        .set_text(&format!("无法搜索：{error}\t\t"))?;
                    if let Some(pane) = session.view.search.borrow().as_ref() {
                        pane.running(false)?;
                    }
                    command = poll_fn(|cx| control.poll(cx)).await;
                    continue 'commands;
                }
                Ok(Ok(batch)) => batch,
            };
            let ready = poll_fn(|cx| {
                if let Poll::Ready(command) = control.poll(cx) {
                    return Poll::Ready(Err(command));
                }
                if session.busy.get() {
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(()))
                }
            })
            .await;
            if let Err(next) = ready {
                command = next;
                continue 'commands;
            }
            if !matches!(&session.browser.borrow().location,Location::Search(Some(current)) if current==&query)
            {
                command = None;
                continue 'commands;
            }
            let done = batch.cursor.done();
            let truncated = batch.cursor.found > model::MAX_ENTRIES;
            let skipped = batch.cursor.skipped;
            let scanned = batch.cursor.scanned;
            let changed = !batch.items.is_empty();
            let found = {
                let mut browser = session.browser.borrow_mut();
                browser.append(batch.items);
                browser.truncated = truncated;
                if changed {
                    session.view.rows(&browser)?;
                }
                browser.items.len()
            };
            let message = if truncated {
                format!("结果超过 {} 项，请缩小搜索范围。", model::MAX_ENTRIES)
            } else if done {
                format!("搜索完毕。找到 {found} 个对象。")
            } else {
                format!("正在搜索… 已检查 {scanned} 个项目，找到 {found} 个对象。")
            };
            let message = if skipped == 0 {
                message
            } else {
                format!("{message} 跳过 {skipped} 个无法读取的项目。")
            };
            if changed || done || progress.elapsed() >= std::time::Duration::from_millis(100) {
                session.view.status.set_text(&format!("{message}\t\t"))?;
                if changed {
                    session.menus()?;
                }
                progress = std::time::Instant::now();
            }
            if done {
                if let Some(pane) = session.view.search.borrow().as_ref() {
                    pane.running(false)?;
                }
                command = poll_fn(|cx| control.poll(cx)).await;
                continue 'commands;
            }
            cursor = Some(batch.cursor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// 实际文件跨块解码/匹配与目录边界，不能由控件或纯字符串断言替代。
    #[test]
    fn streaming_search_preserves_unicode_boundaries_and_does_not_follow_directory_links() {
        let root = std::env::temp_dir().join(format!("liteos-file-search-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let result = (|| -> io::Result<_> {
            fs::create_dir(root.join("child"))?;
            let mut bytes = vec![b'x'; 65538];
            bytes.extend_from_slice("搜Aé中".as_bytes());
            fs::write(root.join("report.TXT"), bytes)?;
            let mut bytes = vec![0xFF, 0xFE];
            for code in std::iter::repeat_n(u16::from(b'x'), 32767).chain("🤝Search".encode_utf16())
            {
                bytes.extend_from_slice(&code.to_le_bytes());
            }
            fs::write(root.join("child").join("unicode.txt"), bytes)?;
            fs::write(root.join("child").join("small.txt"), b"small")?;
            fs::write(root.join("[literal]"), b"no extension")?;
            symlink(&root, root.join("child").join("loop"))?;
            let query = |name: &str, text: &str, recursive| Query {
                name: name.into(),
                text: text.into(),
                location: root.to_string_lossy().into_owned(),
                recursive,
                filters: Default::default(),
            };
            let run = |query| -> io::Result<Vec<PathBuf>> {
                let mut cursor = Cursor::new(query, root.clone())?;
                let mut paths = Vec::new();
                for _ in 0..100 {
                    let batch = cursor.step()?;
                    paths.extend(batch.items.into_iter().filter_map(|item| item.path));
                    cursor = batch.cursor;
                    if cursor.done() {
                        return Ok(paths);
                    }
                }
                Err(io::Error::other("directory link caused an unbounded walk"))
            };
            let mut sensitive = query("*.txt", "🤝search", true);
            sensitive.filters.case_sensitive = true;
            let mismatched = run(sensitive.clone())?;
            sensitive.text = "🤝Search".into();
            let mut small = query("*.txt", "", true);
            small.filters.size = Some((3, "1".into()));
            let mut folders = query("*", "", true);
            folders.filters.kind = Some("文件夹".into());
            Ok((
                run(query("*.txt", "搜aÉ中", true))?,
                run(query("*.TXT", "🤝sEARCH", true))?,
                run(query("[literal]", "", false))?,
                run(query("*.*", "", false))?,
                run(query("*.txt", "🤝search", false))?,
                mismatched,
                run(sensitive)?,
                run(small)?,
                run(folders)?,
            ))
        })();
        fs::remove_dir_all(&root).unwrap();
        let (utf8, utf16, literal, all, shallow, mismatched, sensitive, small, folders) =
            result.unwrap();
        assert_eq!(utf8, [root.join("report.TXT")]);
        assert_eq!(utf16, [root.join("child").join("unicode.txt")]);
        assert_eq!(literal, [root.join("[literal]")]);
        assert!(mismatched.is_empty());
        assert_eq!(sensitive, [root.join("child").join("unicode.txt")]);
        assert_eq!(
            small,
            [root.join("child").join("small.txt")],
            "filtered directories must still be traversed"
        );
        assert_eq!(folders, [root.join("child")]);
        assert!(
            all.contains(&root.join("[literal]")),
            "Windows *.* also matches names without an extension"
        );
        assert!(
            shallow.is_empty(),
            "subfolder option is part of the committed query"
        );
    }
}
