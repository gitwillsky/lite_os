//! 桌面图标：系统图标加 `~/Desktop` 里的条目；按网格摆放，可拖动，位置按用户持久化。
use crate::start::Place;
use crate::{Drag, Rename, icons, popups, start};
use canvas::Rect;
use classic_ui::Edit;
use std::process::Command;
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

/// 第一个网格单元的左上角，逻辑像素；单元尺寸是主题的图标间距（IconSpacing × IconVerticalSpacing）。
const ORIGIN: (i32, i32) = (6, 12);
/// 未保存位置的图标按列自上而下排；每列最多这么多行（对应 768 像素高的屏幕）。
const AUTO_ROWS: i32 = 8;
/// 桌面文件夹里最多显示的条目数。
const MAX_ENTRIES: usize = 256;

/// 点击图标的目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Place(Place),
    Path(PathBuf),
}

pub struct Icon {
    pub label: String,
    /// 32 × 32 位图的名称（不含扩展名）。
    pub art: String,
    pub target: Target,
    /// 持久化位置时的键：系统图标是固定名，其余是文件名。
    key: String,
}

pub struct Icons {
    pub items: Vec<Icon>,
    /// 网格单元尺寸：主题的图标间距，与资源管理器大图标视图共用同一度量。
    cell: (i32, i32),
    /// 与 `items` 一一对应的网格位置 `(列, 行)`。
    cells: Vec<(i32, i32)>,
    saved: HashMap<String, (i32, i32)>,
    directory: PathBuf,
    layout_file: PathBuf,
    stamp: Option<SystemTime>,
    home: PathBuf,
    // Bound the mount-table scan independently of pointer and frame activity.
    next_trash_check: Instant,
    pub trash_full: bool,
}

fn system_icons() -> Vec<Icon> {
    [
        (
            "::documents",
            "我的文档",
            "my-documents-32",
            Place::Documents,
        ),
        (
            "::computer",
            "我的电脑",
            "my-computer-32",
            Place::MyComputer,
        ),
        (
            "::network-places",
            "网上邻居",
            "network-places-32",
            Place::NetworkPlaces,
        ),
        (
            "::recycle-bin",
            "回收站",
            "recycle-bin-empty-32",
            Place::RecycleBin,
        ),
    ]
    .into_iter()
    .map(|(key, label, art, place)| Icon {
        label: label.into(),
        art: art.into(),
        target: Target::Place(place),
        key: key.into(),
    })
    .collect()
}

/// 条目对应的位图：文件夹、文本、可执行程序或通用文件。
fn art_for(path: &Path, directory: bool) -> &'static str {
    if directory {
        return "folder-closed-32";
    }
    let executable = fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
    match path.extension().and_then(|e| e.to_str()) {
        None if executable => "file-app-32",
        _ => "file-generic-32",
    }
}

fn parse_layout(text: &str) -> HashMap<String, (i32, i32)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let key = fields.next()?.to_owned();
            let column = fields.next()?.parse().ok()?;
            let row = fields.next()?.parse().ok()?;
            (column >= 0 && row >= 0).then_some((key, (column, row)))
        })
        .collect()
}

impl Icons {
    /// 读取桌面文件夹（不存在则创建）与保存的位置。
    pub fn load(home: &Path, cell: (i32, i32)) -> Self {
        let directory = home.join("Desktop");
        let _ = fs::create_dir_all(&directory);
        let layout_file = home.join(".config/liteos/desktop-icons");
        let saved = fs::read_to_string(&layout_file)
            .map(|text| parse_layout(&text))
            .unwrap_or_default();
        let mut icons = Self {
            cell: (cell.0.max(1), cell.1.max(1)),
            items: Vec::new(),
            cells: Vec::new(),
            saved,
            directory,
            layout_file,
            stamp: None,
            home: home.to_owned(),
            next_trash_check: Instant::now(),
            trash_full: false,
        };
        icons.refresh();
        icons.poll_trash();
        icons
    }

    fn modified(&self) -> Option<SystemTime> {
        fs::metadata(&self.directory)
            .and_then(|m| m.modified())
            .ok()
    }

    /// 桌面文件夹自上次读取后是否变化。
    pub fn changed(&self) -> bool {
        self.modified() != self.stamp
    }

    /// 回收站满/空状态变化时返回 true；目录不存在的账户使用空图标。
    pub fn poll_trash(&mut self) -> bool {
        let now = Instant::now();
        if now < self.next_trash_check {
            return false;
        }
        self.next_trash_check = now + Duration::from_secs(1);
        let Ok(full) = desktop_entry::trash::has_items(&self.home) else {
            return false;
        };
        std::mem::replace(&mut self.trash_full, full) != full
    }

    /// 重新读取桌面文件夹并重新分配位置：保存过且仍空闲的用保存位置，其余按列顺序填空格。
    pub fn refresh(&mut self) {
        self.stamp = self.modified();
        let associations = desktop_entry::Associations::load();
        let mut items = system_icons();
        let mut entries: Vec<(bool, String, PathBuf)> = fs::read_dir(&self.directory)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .take(MAX_ENTRIES)
            .map(|e| {
                let path = e.path();
                (
                    path.is_dir(),
                    e.file_name().to_string_lossy().into_owned(),
                    path,
                )
            })
            .collect();
        entries.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        });
        for (directory, name, path) in entries {
            let entry = (path
                .extension()
                .is_some_and(|extension| extension == "desktop"))
            .then(|| desktop_entry::read(&path).ok())
            .flatten();
            items.push(Icon {
                art: entry
                    .as_ref()
                    .and_then(|entry| entry.icon.as_deref())
                    .or_else(|| (!directory).then(|| associations.icon_for(&path)).flatten())
                    .map_or_else(
                        || art_for(&path, directory).to_owned(),
                        |icon| format!("{icon}-32"),
                    ),
                label: entry
                    .as_ref()
                    .map_or_else(|| name.clone(), |entry| entry.name.clone()),
                target: Target::Path(path),
                key: name,
            });
        }
        let mut cells: Vec<Option<(i32, i32)>> = vec![None; items.len()];
        let mut taken = Vec::new();
        for (index, icon) in items.iter().enumerate() {
            if let Some(cell) = self.saved.get(&icon.key).filter(|c| !taken.contains(*c)) {
                cells[index] = Some(*cell);
                taken.push(*cell);
            }
        }
        let mut next = 0;
        for slot in cells.iter_mut().filter(|slot| slot.is_none()) {
            let cell = loop {
                let cell = (next / AUTO_ROWS, next % AUTO_ROWS);
                next += 1;
                if !taken.contains(&cell) {
                    break cell;
                }
            };
            taken.push(cell);
            *slot = Some(cell);
        }
        self.cells = cells.into_iter().flatten().collect();
        self.items = items;
    }

    pub fn rect(&self, index: usize) -> Rect {
        let (column, row) = self.cells[index];
        Rect::new(
            ORIGIN.0 + column * self.cell.0,
            ORIGIN.1 + row * self.cell.1,
            self.cell.0,
            self.cell.1,
        )
    }

    /// 图标视图的 Shift 范围是两端网格围成的矩形；自由移动后不能按目录行号取连续项。
    /// 参数为仍有效的锚点和目标索引，返回矩形内按条目索引排序的项目，不分配集合。
    pub fn rectangular_selection(
        &self,
        anchor: usize,
        target: usize,
    ) -> impl Iterator<Item = usize> + '_ {
        let (a, b) = (self.cells[anchor], self.cells[target]);
        self.cells
            .iter()
            .enumerate()
            .filter_map(move |(index, &(x, y))| {
                ((a.0.min(b.0)..=a.0.max(b.0)).contains(&x)
                    && (a.1.min(b.1)..=a.1.max(b.1)).contains(&y))
                .then_some(index)
            })
    }

    /// 在 `work_area` 内按网格移动选择。方向是单轴的一格偏移。
    /// 无选择时返回最左列的第一个项目，到达边缘时保留当前项。
    pub fn step(
        &self,
        current: Option<usize>,
        direction: (i32, i32),
        work_area: (i32, i32),
    ) -> Option<usize> {
        let (width, height) = (work_area.0 - ORIGIN.0, work_area.1 - ORIGIN.1);
        let visible = |&(column, row): &(i32, i32)| {
            i64::from(column) * i64::from(self.cell.0) < i64::from(width)
                && i64::from(row) * i64::from(self.cell.1) < i64::from(height)
        };
        let candidates = self
            .cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| visible(cell));
        let Some((index, &(column, row))) = current
            .and_then(|index| self.cells.get(index).map(|cell| (index, cell)))
            .filter(|(_, cell)| visible(cell))
        else {
            return candidates.min_by_key(|(_, cell)| **cell).map(|(i, _)| i);
        };
        candidates
            .filter_map(|(i, &(x, y))| {
                let (dx, dy) = (x - column, y - row);
                if dx * direction.0 + dy * direction.1 <= 0 {
                    return None;
                }
                // 从相邻格的中心比较距离，允许跨过空格和不齐的列。
                let x = i64::from(dx - direction.0) * i64::from(self.cell.0);
                let y = i64::from(dy - direction.1) * i64::from(self.cell.1);
                Some((x * x + y * y, i))
            })
            .min()
            .map(|(_, i)| i)
            .or(Some(index))
    }

    /// 按抓取项的中心 `center` 平移整组选中图标，保留相对网格位置，避开未选中项后保存。
    ///
    /// # Errors
    ///
    /// 无效选择或桌面没有容纳整组的空间时保持原位；保存失败时内存位置已更新。
    pub fn drop_at(
        &mut self,
        selection: &[usize],
        index: usize,
        center: (i32, i32),
        screen: (i32, i32),
    ) -> io::Result<()> {
        if selection.binary_search(&index).is_err()
            || selection.iter().any(|i| *i >= self.cells.len())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "无效的图标选择",
            ));
        }
        let columns = ((screen.0 - ORIGIN.0) / self.cell.0).max(1);
        let rows = ((screen.1 - 28 - ORIGIN.1) / self.cell.1).max(1);
        let origin = self.cells[index];
        let wanted = (
            (center.0 - ORIGIN.0) / self.cell.0 - origin.0,
            (center.1 - ORIGIN.1) / self.cell.1 - origin.1,
        );
        let bounds = selection.iter().fold(
            (origin.0, origin.1, origin.0, origin.1),
            |(left, top, right, bottom), i| {
                let (x, y) = self.cells[*i];
                (left.min(x), top.min(y), right.max(x), bottom.max(y))
            },
        );
        // 只在释放抓取时建立局部占用索引。移动事件不分配，不逐项放置而打散整组的相对位置。
        let occupied: HashSet<_> = self
            .cells
            .iter()
            .enumerate()
            .filter(|(i, _)| selection.binary_search(i).is_err())
            .map(|(_, cell)| *cell)
            .collect();
        let free = |&(dx, dy): &(i32, i32)| {
            selection.iter().all(|i| {
                let (x, y) = self.cells[*i];
                !occupied.contains(&(x + dx, y + dy))
            })
        };
        let target = (-bounds.0..columns - bounds.2)
            .flat_map(|dx| (-bounds.1..rows - bounds.3).map(move |dy| (dx, dy)))
            .filter(free)
            .min_by_key(|(dx, dy)| {
                (i64::from(*dx) - i64::from(wanted.0)).pow(2)
                    + (i64::from(*dy) - i64::from(wanted.1)).pow(2)
            })
            .ok_or_else(|| io::Error::other("桌面空间不足，无法保持所选图标的排列"))?;
        for i in selection {
            self.cells[*i].0 += target.0;
            self.cells[*i].1 += target.1;
        }
        for (icon, cell) in self.items.iter().zip(&self.cells) {
            self.saved.insert(icon.key.clone(), *cell);
        }
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.layout_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let text: String = self
            .saved
            .iter()
            .map(|(key, (column, row))| format!("{key}\t{column}\t{row}\n"))
            .collect();
        fs::write(&self.layout_file, text)
    }

    /// 同目录改名并保留原来的网格位置；文件系统冲突由原子 rename 裁决。
    pub fn rename(
        &mut self,
        draft: &desktop_entry::fileops::Rename,
        name: &str,
    ) -> io::Result<PathBuf> {
        let path = draft.path();
        let target = draft.commit(name)?;
        if target == path {
            return Ok(target);
        }
        if let Some(index) = self
            .items
            .iter()
            .position(|icon| matches!(&icon.target, Target::Path(current) if current == path))
        {
            for (icon, cell) in self.items.iter().zip(&self.cells) {
                self.saved.insert(icon.key.clone(), *cell);
            }
            let old = &self.items[index].key;
            let cell = self.cells[index];
            self.saved.remove(old);
            self.saved.insert(name.to_owned(), cell);
        }
        self.refresh();
        if let Err(error) = self.save() {
            eprintln!("desktop: cannot persist renamed icon position: {error}");
        }
        Ok(target)
    }

    /// 忘记所有保存的位置，图标按名称顺序重新排列。
    pub fn auto_arrange(&mut self) -> io::Result<()> {
        match fs::remove_file(&self.layout_file) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        self.saved.clear();
        Ok(())
    }

    /// 在桌面文件夹里新建"新建文件夹"（重名加序号）。
    pub fn new_folder(&self) -> io::Result<PathBuf> {
        desktop_entry::fileops::new_folder(&self.directory)
    }
}

// Desktop 保留唯一交互状态；图标动作只通过该 owner 修改选择、重命名和刷新。
impl crate::Desktop {
    pub(super) fn icon_kind(&self, index: usize) -> popups::IconKind {
        match self.icons.items.get(index).map(|icon| &icon.target) {
            Some(icons::Target::Path(path)) if path.is_file() => popups::IconKind::File,
            Some(icons::Target::Path(_)) => popups::IconKind::Path,
            Some(icons::Target::Place(start::Place::MyComputer)) => popups::IconKind::MyComputer,
            Some(icons::Target::Place(start::Place::NetworkPlaces)) => {
                popups::IconKind::NetworkPlaces
            }
            Some(icons::Target::Place(start::Place::RecycleBin)) => popups::IconKind::RecycleBin {
                full: self.icons.trash_full,
            },
            _ => popups::IconKind::Other,
        }
    }

    /// 让资源管理器对桌面上的文件或文件夹执行动词：打开它所在的文件夹、选中它、立即执行。
    pub(super) fn icon_verb(&mut self, index: usize, verb: &str) {
        let Some(icons::Target::Path(path)) =
            self.icons.items.get(index).map(|icon| icon.target.clone())
        else {
            return;
        };
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        let mut command = Command::new("/bin/file-explorer");
        command
            .arg(parent)
            .args(["--select"])
            .arg(name)
            .args(["--verb", verb]);
        self.launch(command);
    }

    pub(super) fn select_icon(&mut self, index: usize, shift: bool, control: bool, toggle: bool) {
        if shift {
            let anchor = self.selection_anchor.or(self.focused).unwrap_or(index);
            if !control {
                self.selection.clear();
            }
            self.selection
                .extend(self.icons.rectangular_selection(anchor, index));
            self.selection.sort_unstable();
            self.selection.dedup();
            self.selection_anchor = Some(anchor);
        } else if control {
            if toggle {
                match self.selection.binary_search(&index) {
                    Ok(at) => {
                        self.selection.remove(at);
                    }
                    Err(at) => self.selection.insert(at, index),
                }
                self.selection_anchor = Some(index);
            }
        } else {
            self.selection.clear();
            self.selection.push(index);
            self.selection_anchor = Some(index);
        }
        self.focused = Some(index);
    }

    pub(super) fn selected_paths(&self) -> Vec<PathBuf> {
        self.selection
            .iter()
            .filter_map(|index| match &self.icons.items[*index].target {
                icons::Target::Path(path) => Some(path.clone()),
                icons::Target::Place(_) => None,
            })
            .collect()
    }

    pub(super) fn cancel_drag(&mut self) {
        if let Some(Drag::Selection { before, .. }) = self.drag.take() {
            self.selection = before;
        }
        self.armed = None;
        self.clicked = None;
    }

    pub(super) fn rename_icon(&mut self, index: usize) {
        let Some(icons::Target::Path(path)) = self.icons.items.get(index).map(|icon| &icon.target)
        else {
            return;
        };
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            return;
        };
        let mut edit = Edit::new(name);
        if edit.text() != name {
            return;
        }
        let operation = match desktop_entry::fileops::Rename::prepare(path.clone()) {
            Ok(operation) => operation,
            Err(error) => {
                self.message = format!("无法重命名：{error}");
                return;
            }
        };
        edit.select_position(0, false);
        edit.select_position(name.len(), true);
        self.select_icon(index, false, false, false);
        self.clicked = None;
        self.rename = Some(Rename {
            operation,
            edit,
            selecting: false,
        });
        self.message.clear();
    }

    pub(super) fn refresh_icons(&mut self) {
        // 粘贴完成也可能发起刷新；先结束旧索引的抓取，再按目标身份重建选择。
        self.cancel_drag();
        let selected = self
            .focused
            .and_then(|index| self.icons.items.get(index))
            .map(|icon| icon.target.clone());
        let selection: Vec<_> = self
            .selection
            .iter()
            .filter_map(|index| self.icons.items.get(*index))
            .map(|icon| icon.target.clone())
            .collect();
        let anchor = self
            .selection_anchor
            .and_then(|index| self.icons.items.get(index))
            .map(|icon| icon.target.clone());
        self.icons.refresh();
        self.focused = selected.and_then(|target| {
            self.icons
                .items
                .iter()
                .position(|icon| icon.target == target)
        });
        self.selection.clear();
        for (index, icon) in self.icons.items.iter().enumerate() {
            if selection.contains(&icon.target) {
                self.selection.push(index);
            }
        }
        self.selection_anchor = anchor.and_then(|target| {
            self.icons
                .items
                .iter()
                .position(|icon| icon.target == target)
        });
        self.clicked = None;
        self.rename_click = None;
        self.apps_changed = true;
        if let Some(rename) = &self.rename {
            let visible = self.icons.items.iter().any(|icon| {
                matches!(&icon.target, icons::Target::Path(path) if path == rename.operation.path())
            });
            let error = match rename.operation.verify() {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Some(error.to_string()),
                _ if !visible => Some("正在编辑的项目已不在桌面上，请重新选择。".into()),
                _ => None,
            };
            if let Some(error) = error {
                self.rename = None;
                self.focused = None;
                self.selection.clear();
                self.selection_anchor = None;
                self.message = format!("无法重命名：{error}");
            }
        }
    }

    pub(super) fn commit_rename(&mut self) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        match self.icons.rename(&rename.operation, rename.edit.text()) {
            Ok(path) => {
                self.message.clear();
                if let Some(index) = self.icons.items.iter().position(
                    |icon| matches!(&icon.target, icons::Target::Path(current) if *current == path),
                ) {
                    self.select_icon(index, false, false, false);
                }
                self.apps_changed = true;
            }
            Err(error) => {
                self.message = format!("无法重命名：{error}");
                if error.kind() == io::ErrorKind::NotFound {
                    self.focused = None;
                    self.selection.clear();
                    self.selection_anchor = None;
                    self.refresh_icons();
                } else {
                    self.rename = Some(rename);
                }
            }
        }
    }

    pub(super) fn delete_selected(&mut self, verb: &str) {
        let paths = self.selected_paths();
        if !paths.is_empty()
            && let Err(error) = self.delete_paths(&paths, verb)
        {
            self.message = format!("无法删除：{error}");
        }
        self.clicked = None;
    }
}
