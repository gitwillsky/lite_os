//! 桌面图标：三个系统图标加 `~/Desktop` 里的条目；按网格摆放，可拖动，位置按用户持久化。
use crate::start::Place;
use canvas::Rect;
use std::{
    collections::HashMap,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::SystemTime,
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
    pub art: &'static str,
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
}

fn system_icons() -> Vec<Icon> {
    [
        ("::documents", "我的文档", "documents-32", Place::Documents),
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
        art,
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
        Some("txt" | "log" | "md" | "conf" | "ini") => "file-text-32",
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
        };
        icons.refresh();
        icons
    }

    fn modified(&self) -> Option<SystemTime> {
        fs::metadata(&self.directory)
            .and_then(|m| m.modified())
            .ok()
    }

    /// 桌面文件夹自上次读取后是否变化；变化时重读。返回是否需要重绘。
    pub fn poll(&mut self) -> bool {
        if self.modified() == self.stamp {
            return false;
        }
        self.refresh();
        true
    }

    /// 重新读取桌面文件夹并重新分配位置：保存过且仍空闲的用保存位置，其余按列顺序填空格。
    pub fn refresh(&mut self) {
        self.stamp = self.modified();
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
            items.push(Icon {
                art: art_for(&path, directory),
                label: name.clone(),
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

    /// 把图标拖放到屏幕位置 `center`（图标中心）：落在被占用的格子上则选最近的空格；保存新位置。
    ///
    /// # Errors
    ///
    /// 保存位置文件失败；内存中的位置已更新，下次启动才会丢失。
    pub fn drop_at(
        &mut self,
        index: usize,
        center: (i32, i32),
        screen: (i32, i32),
    ) -> io::Result<()> {
        let columns = ((screen.0 - ORIGIN.0) / self.cell.0).max(1);
        let rows = ((screen.1 - 28 - ORIGIN.1) / self.cell.1).max(1);
        let wanted = (
            ((center.0 - ORIGIN.0) / self.cell.0).clamp(0, columns - 1),
            ((center.1 - ORIGIN.1) / self.cell.1).clamp(0, rows - 1),
        );
        let free = |cell: &(i32, i32)| {
            !self
                .cells
                .iter()
                .enumerate()
                .any(|(i, c)| i != index && c == cell)
        };
        let target = (0..columns)
            .flat_map(|c| (0..rows).map(move |r| (c, r)))
            .filter(free)
            .min_by_key(|(c, r)| (c - wanted.0).pow(2) + (r - wanted.1).pow(2))
            .unwrap_or(wanted);
        self.cells[index] = target;
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

    /// 忘记所有保存的位置，图标按名称顺序重新排列。
    pub fn auto_arrange(&mut self) -> io::Result<()> {
        self.saved.clear();
        self.refresh();
        match fs::remove_file(&self.layout_file) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// 在桌面文件夹里新建"新建文件夹"（重名加序号）。
    pub fn new_folder(&mut self) -> io::Result<()> {
        let mut path = self.directory.join("新建文件夹");
        for n in 2.. {
            match fs::create_dir(&path) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    path = self.directory.join(format!("新建文件夹 ({n})"));
                }
                result => {
                    result?;
                    break;
                }
            }
        }
        self.refresh();
        Ok(())
    }
}
