//! 资源管理器的导航状态：当前位置、前进/后退历史、文件夹树与选择。
use crate::{
    fileops,
    grid::ViewMode,
    model::{Icon, Item, Location},
};
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

/// 文件夹树的一行。
pub struct TreeRow {
    pub location: Location,
    pub label: String,
    pub icon: Icon,
    pub depth: i32,
    pub expandable: bool,
    pub expanded: bool,
}

/// 剪切/复制的内容；只在本进程内有效（没有跨应用剪贴板协议）。
pub struct Clipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

pub struct Browser {
    home: PathBuf,
    pub clipboard: Option<Clipboard>,
    /// 除 `selected`（焦点项，也是区间选择的锚点）以外被选中的项；Ctrl 点击与 Shift 区间选择产生。
    pub extra: std::collections::BTreeSet<usize>,
    /// `/proc/mounts` 文本，导航与刷新时重读。
    mounts: String,
    pub location: Location,
    history: Vec<Location>,
    cursor: usize,
    pub items: Vec<Item>,
    /// 目录项超过上限被截断。
    pub truncated: bool,
    /// 当前位置无法读取或打开项目失败的原因；下一次导航时清除。
    pub message: Option<String>,
    pub selected: Option<usize>,
    pub view: ViewMode,
    pub show_toolbar: bool,
    pub show_status_bar: bool,
    pub show_folders: bool,
    /// 文件夹树已展开的位置；树的行由它与 `children` 缓存推出。
    expanded: HashSet<Location>,
    /// 子文件夹缓存，避免每帧读目录；刷新时清空。
    children: HashMap<Location, Vec<(String, Icon, Location)>>,
    pub rows: Vec<TreeRow>,
}

impl Browser {
    pub fn new(location: Location) -> Self {
        Self::with_mounts(
            location,
            fs::read_to_string("/proc/mounts").unwrap_or_default(),
        )
    }

    /// `mounts` 是 `/proc/mounts` 的文本；单独传入使测试不依赖宿主机的挂载表。
    fn with_mounts(location: Location, mounts: String) -> Self {
        let mut browser = Self {
            home: std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from),
            clipboard: None,
            mounts,
            history: vec![location.clone()],
            location,
            cursor: 0,
            items: Vec::new(),
            truncated: false,
            message: None,
            selected: None,
            extra: std::collections::BTreeSet::new(),
            view: ViewMode::Details,
            show_toolbar: true,
            show_status_bar: true,
            show_folders: false,
            expanded: HashSet::new(),
            children: HashMap::new(),
            rows: Vec::new(),
        };
        browser.load();
        browser
    }

    /// 重新读取当前位置并刷新文件夹树；选择被清除。
    fn load(&mut self) {
        self.selected = None;
        self.extra.clear();
        match self.location.list(&self.mounts) {
            Ok((items, truncated)) => {
                self.items = items;
                self.truncated = truncated;
            }
            Err(error) => {
                self.items.clear();
                self.truncated = false;
                self.message = Some(format!("无法访问 {}：{error}", self.location.address()));
            }
        }
        let mut place = Some(self.location.clone());
        while let Some(current) = place {
            place = current.parent();
            self.expanded.insert(current);
        }
        self.rebuild_rows();
    }

    pub fn refresh(&mut self) {
        self.children.clear();
        self.message = None;
        self.mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
        self.load();
    }

    /// 跳转到新位置并截断前进历史。
    pub fn go(&mut self, location: Location) {
        if location == self.location {
            return;
        }
        self.history.truncate(self.cursor + 1);
        self.history.push(location.clone());
        self.cursor += 1;
        self.location = location;
        self.message = None;
        self.load();
    }

    /// 当前位置是不是回收站目录。
    pub fn in_trash(&self) -> bool {
        matches!(&self.location, Location::Directory(p) if *p == fileops::trash_files(&self.home))
    }

    /// 是否可以在当前位置新建、粘贴、删除（只有真实目录可以）。
    pub fn can_modify(&self) -> bool {
        matches!(self.location, Location::Directory(_))
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.items.get(self.selected?)?.path.clone()
    }

    /// 选中名为 `name` 的项；没有这一项时什么也不选。
    pub fn select_named(&mut self, name: &str) {
        if let Some(index) = self.items.iter().position(|item| item.name == name) {
            self.select_only(index);
        }
    }

    /// 全部选中项的序号，升序：焦点项加多选的其余项。
    pub fn selection(&self) -> Vec<usize> {
        let mut all: Vec<usize> = self.extra.iter().copied().chain(self.selected).collect();
        all.sort_unstable();
        all.dedup();
        all
    }

    pub fn is_selected(&self, index: usize) -> bool {
        self.selected == Some(index) || self.extra.contains(&index)
    }

    /// 全部选中项的真实路径（虚拟项如"我的电脑"没有路径，被跳过）。
    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selection()
            .into_iter()
            .filter_map(|i| self.items.get(i)?.path.clone())
            .collect()
    }

    /// 只选中 `index`。
    pub fn select_only(&mut self, index: usize) {
        self.extra.clear();
        self.selected = Some(index);
    }

    /// Ctrl 点击：切换 `index` 的选中状态；焦点移到它（取消选中时焦点留给剩下的某一项）。
    pub fn toggle(&mut self, index: usize) {
        if self.is_selected(index) {
            self.extra.remove(&index);
            if self.selected == Some(index) {
                self.selected = self.extra.pop_first();
            }
        } else {
            if let Some(previous) = self.selected {
                self.extra.insert(previous);
            }
            self.selected = Some(index);
        }
    }

    /// Shift 点击：从焦点项（锚点）到 `index` 的整段都选中，焦点留在锚点。
    pub fn select_range(&mut self, index: usize) {
        let anchor = self.selected.unwrap_or(index);
        self.extra = (anchor.min(index)..=anchor.max(index))
            .filter(|i| *i != anchor)
            .collect();
        self.selected = Some(anchor);
    }

    pub fn select_all(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = Some(0);
        self.extra = (1..self.items.len()).collect();
    }

    /// 重新读取当前位置，保留状态栏消息；用于文件操作之后。
    fn reload(&mut self) {
        self.children.clear();
        self.load();
    }

    /// 记录操作结果：失败写入状态栏，成功清除旧消息；随后重读目录。
    fn finish(&mut self, result: io::Result<()>) {
        self.message = result.err().map(|error| error.to_string());
        self.reload();
    }

    fn select_path(&mut self, path: &Path) {
        self.selected = self
            .items
            .iter()
            .position(|item| item.path.as_deref() == Some(path));
    }

    /// 为选中项打开独立的"属性"窗口（控制面板的属性页）；没有选择时什么也不做。
    ///
    /// 图标与类型文本由本模块的分类决定，作为参数交给属性页，避免两处各维护一份类型名。
    pub fn show_properties(&mut self) {
        let Some(item) = self.selected.and_then(|i| self.items.get(i)).cloned() else {
            return;
        };
        let Some(path) = item.path else { return };
        let mut command = Command::new("/bin/control-panel");
        command
            .arg("properties")
            .arg(&path)
            .arg(item.icon.file_stem())
            .arg(&item.kind);
        if matches!(item.icon, Icon::Disk | Icon::OpticalDisc) {
            command.arg("drive");
        }
        self.message = command
            .env_remove("WAYLAND_SOCKET")
            .spawn()
            .map(drop)
            .err()
            .map(|error| format!("无法打开 {} 的属性：{error}", item.name));
    }

    /// 删除全部选中项；遇到第一个失败就停下并报告（已删的不回滚）。
    pub fn delete_selected(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let result = paths
            .iter()
            .try_for_each(|path| fileops::delete(&self.home, path));
        self.finish(result);
    }

    pub fn copy_selected(&mut self, cut: bool) {
        let paths = self.selected_paths();
        self.clipboard = (!paths.is_empty()).then_some(Clipboard { paths, cut });
    }

    pub fn paste(&mut self) {
        let (Some(clip), Location::Directory(dir)) = (self.clipboard.take(), self.location.clone())
        else {
            return;
        };
        // 复制保留剪贴板以便再次粘贴；剪切在粘贴后用掉。
        if !clip.cut {
            self.clipboard = Some(Clipboard {
                paths: clip.paths.clone(),
                cut: false,
            });
        }
        let mut last = None;
        for path in &clip.paths {
            let result = if clip.cut {
                fileops::relocate(path, &dir)
            } else {
                fileops::copy(path, &dir)
            };
            match result {
                Ok(target) => last = Some(target),
                Err(error) => return self.finish(Err(error)),
            }
        }
        self.finish(Ok(()));
        if let Some(path) = last {
            self.select_path(&path);
        }
    }

    /// 重命名选中项；失败时返回 `false`，调用者可保持编辑状态。
    pub fn rename_selected(&mut self, name: &str) -> bool {
        let Some(path) = self.selected_path() else {
            return false;
        };
        match fileops::rename(&path, name) {
            Ok(target) => {
                self.finish(Ok(()));
                self.select_path(&target);
                true
            }
            Err(error) => {
                self.message = Some(error.to_string());
                false
            }
        }
    }

    /// 新建文件夹并选中它，返回它的序号以便立即进入重命名。
    pub fn new_folder(&mut self) -> Option<usize> {
        let Location::Directory(dir) = self.location.clone() else {
            return None;
        };
        match fileops::new_folder(&dir) {
            Ok(path) => {
                self.finish(Ok(()));
                self.select_path(&path);
                self.selected
            }
            Err(error) => {
                self.finish(Err(error));
                None
            }
        }
    }

    pub fn restore_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        let result = fileops::restore(&self.home, &path).map(drop);
        self.finish(result);
    }

    pub fn empty_trash(&mut self) {
        let result = fileops::empty_trash(&self.home).map(drop);
        self.finish(result);
    }

    pub fn can_back(&self) -> bool {
        self.cursor > 0
    }

    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.history.len()
    }

    pub fn back(&mut self) {
        if self.can_back() {
            self.cursor -= 1;
            self.visit_history();
        }
    }

    pub fn forward(&mut self) {
        if self.can_forward() {
            self.cursor += 1;
            self.visit_history();
        }
    }

    fn visit_history(&mut self) {
        self.location = self.history[self.cursor].clone();
        self.message = None;
        self.load();
    }

    pub fn up(&mut self) {
        if let Some(parent) = self.location.parent() {
            self.go(parent);
        }
    }

    /// 打开选中项：文件夹就地导航；可执行文件启动；其余文件没有关联程序，给出提示。
    pub fn open_selected(&mut self) {
        let Some(item) = self.selected.and_then(|i| self.items.get(i)).cloned() else {
            return;
        };
        if let Some(target) = item.target {
            self.go(target);
            return;
        }
        if let Some(launch) = &item.launch {
            let launched = Command::new(&launch[0])
                .args(&launch[1..])
                .env_remove("WAYLAND_SOCKET")
                .spawn()
                .map(drop);
            self.message = launched
                .err()
                .map(|error| format!("无法启动 {}：{error}", item.name));
            return;
        }
        let Some(path) = item.path else { return };
        if item.icon == Icon::Application {
            let launched = Command::new(&path)
                .env_remove("WAYLAND_SOCKET")
                .spawn()
                .map(drop);
            self.message = launched
                .err()
                .map(|error| format!("无法启动 {}：{error}", item.name));
        } else if let Some(mut command) =
            desktop_entry::open_with(Path::new("/usr/share/applications"), &path)
        {
            let launched = command.env_remove("WAYLAND_SOCKET").spawn().map(drop);
            self.message = launched
                .err()
                .map(|error| format!("无法打开 {}：{error}", item.name));
        } else {
            self.message = Some(format!("没有与 {} 关联的程序。", item.name));
        }
    }

    fn subfolders(&mut self, location: &Location) -> &[(String, Icon, Location)] {
        let mounts = &self.mounts;
        self.children.entry(location.clone()).or_insert_with(|| {
            location
                .list(mounts)
                .map(|(items, _)| {
                    items
                        .into_iter()
                        .filter_map(|item| Some((item.name, item.icon, item.target?)))
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// 切换树节点展开状态。
    pub fn toggle_expanded(&mut self, location: &Location) {
        if !self.expanded.remove(location) {
            self.expanded.insert(location.clone());
        }
        self.rebuild_rows();
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        // 栈顶先弹出：后压入的“我的电脑”显示在“网上邻居”之前。
        let mut pending = vec![
            (
                Location::NetworkPlaces,
                "网上邻居".to_owned(),
                Icon::NetworkPlaces,
                0,
            ),
            (
                Location::MyComputer,
                "我的电脑".to_owned(),
                Icon::MyComputer,
                0,
            ),
        ];
        // 栈式前序遍历：子节点逆序压栈，弹出顺序与显示顺序一致。
        while let Some((location, label, icon, depth)) = pending.pop() {
            let children: Vec<_> = self.subfolders(&location).to_vec();
            let expanded = self.expanded.contains(&location);
            rows.push(TreeRow {
                expandable: !children.is_empty(),
                expanded: expanded && !children.is_empty(),
                location,
                label,
                icon,
                depth,
            });
            if expanded {
                for (label, icon, child) in children.into_iter().rev() {
                    pending.push((child, label, icon, depth + 1));
                }
            }
        }
        self.rows = rows;
    }

    pub fn tree_index(&self, location: &Location) -> Option<usize> {
        self.rows.iter().position(|row| &row.location == location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    const MOUNTS: &str = "/dev/vda / ext4 rw 0 0\n";

    fn scratch(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("file-explorer-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        fs::write(root.join("a/readme.txt"), "x").unwrap();
        root
    }

    #[test]
    fn history_supports_back_forward_and_drops_forward_on_new_visit() {
        let root = scratch("history");
        let at = |p: &str| Location::Directory(root.join(p));
        let mut browser = Browser::with_mounts(at("a"), MOUNTS.into());
        browser.go(at("a/b"));
        browser.back();
        assert_eq!(browser.location, at("a"));
        assert!(browser.can_forward());
        browser.go(at("c"));
        assert!(
            !browser.can_forward(),
            "a new visit discards the forward history"
        );
        browser.back();
        assert_eq!(browser.location, at("a"));
        browser.up();
        assert_eq!(browser.location, Location::Directory(root.clone()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn listing_puts_folders_first_and_describes_files() {
        let root = scratch("listing");
        let browser = Browser::with_mounts(Location::Directory(root.join("a")), MOUNTS.into());
        let names: Vec<_> = browser.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["b", "readme.txt"]);
        assert_eq!(browser.items[1].kind, "文本文档");
        assert_eq!(browser.items[1].size, Some(1));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_tree_reveals_the_current_location_and_collapses_on_toggle() {
        let root = scratch("tree");
        let here = Location::Directory(root.join("a/b"));
        let mut browser = Browser::with_mounts(
            here.clone(),
            // 把临时目录当作一个磁盘的挂载点，树的前几层就与宿主机的真实目录无关。
            format!("/dev/vda {} ext4 rw 0 0\n", root.display()),
        );
        let index = browser
            .tree_index(&here)
            .expect("current folder is revealed");
        assert_eq!(browser.rows[0].location, Location::MyComputer);
        assert!(browser.rows[index].depth > 1);
        let parent = Location::Directory(root.join("a"));
        browser.toggle_expanded(&parent);
        assert!(browser.tree_index(&here).is_none());
        assert!(browser.tree_index(&parent).is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn opening_a_plain_file_explains_instead_of_failing_silently() {
        let root = scratch("open");
        let mut browser = Browser::with_mounts(Location::Directory(root.join("a")), MOUNTS.into());
        browser.selected = Some(1);
        browser.open_selected();
        assert!(browser.message.as_deref().unwrap().contains("readme.txt"));
        browser.selected = Some(0);
        browser.open_selected();
        assert_eq!(browser.location, Location::Directory(root.join("a/b")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copy_paste_rename_and_new_folder_update_the_listing_and_selection() {
        let root = scratch("ops");
        let mut browser = Browser::with_mounts(Location::Directory(root.join("a")), MOUNTS.into());
        browser.selected = Some(1);
        browser.copy_selected(false);
        browser.paste();
        let names: Vec<_> = browser.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["b", "readme 的副本.txt", "readme.txt"]);
        assert_eq!(
            browser.items[browser.selected.unwrap()].name,
            "readme 的副本.txt"
        );
        assert!(
            browser.clipboard.is_some(),
            "copy keeps the clipboard for further pastes"
        );
        assert!(!browser.rename_selected("b"), "an existing name is refused");
        assert!(browser.message.is_some());
        assert!(browser.rename_selected("notes.txt"));
        assert_eq!(browser.items[browser.selected.unwrap()].name, "notes.txt");
        let index = browser.new_folder().unwrap();
        assert_eq!(browser.items[index].name, "新建文件夹");
        browser.copy_selected(true);
        browser.location = Location::Directory(root.join("c"));
        browser.load();
        browser.paste();
        assert!(root.join("c/新建文件夹").is_dir() && !root.join("a/新建文件夹").exists());
        assert!(browser.clipboard.is_none(), "cut is consumed by the paste");
        let _ = fs::remove_dir_all(root);
    }
}
