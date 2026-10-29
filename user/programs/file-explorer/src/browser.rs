//! 文件浏览的领域数据：目录快照、路径身份、导航历史和文件夹目录；不保存控件选择、展开或滚动。
use crate::{
    fileops,
    model::{Field, Icon, Item, Location},
};
use std::{
    collections::{BTreeMap, HashMap},
    fs, io,
    path::{Path, PathBuf},
    process::{Child, Command},
};
pub struct Folder {
    pub location: Location,
    pub name: String,
    pub icon: Icon,
}
pub struct Branch {
    pub location: Location,
    pub folders: Vec<Folder>,
}
pub struct Listing {
    pub location: Location,
    pub items: Vec<Item>,
    pub truncated: bool,
    /// 真实目录所在文件系统的可用空间（状态栏“可用磁盘空间”）。
    pub free: Option<u64>,
    pub branches: Vec<Branch>,
}
fn folders(location: &Location, mounts: &str) -> io::Result<Vec<Folder>> {
    if let Location::Directory(path) = location {
        // 侧栏只需要目录，不为祖先目录中的每个文件读取大小/时间或扫描文件关联。
        let mut result = Vec::new();
        for entry in fs::read_dir(path)?.take(crate::model::MAX_ENTRIES) {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()) {
                result.push(Folder {
                    location: Location::Directory(entry.path()),
                    name: entry.file_name().to_string_lossy().into_owned(),
                    icon: Icon::Folder,
                });
            }
        }
        result.sort_by_key(|folder| folder.name.to_lowercase());
        return Ok(result);
    }
    Ok(location
        .list(mounts)?
        .0
        .into_iter()
        .filter_map(|item| {
            Some(Folder {
                location: item.target?,
                name: item.name,
                icon: item.icon,
            })
        })
        .collect())
}
pub fn read_branch(location: Location) -> io::Result<Branch> {
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    Ok(Branch {
        folders: folders(&location, &mounts)?,
        location,
    })
}
pub fn read(mut location: Location, home: &Path) -> io::Result<Listing> {
    if let Location::Directory(path) = &mut location {
        if *path == fileops::trash_files(home) {
            fs::create_dir_all(&*path)?;
        }
        *path = fs::canonicalize(&*path)?;
    }
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    let (items, truncated) = location.list(&mounts)?;
    let free = match &location {
        Location::Directory(path) => linux_uapi::filesystem::capacity(path)
            .ok()
            .map(|capacity| capacity.free),
        _ => None,
    };
    let mut places = Vec::new();
    let mut parent = Some(location.clone());
    // Tree 的深度上限只限制目录侧栏；深层真实路径仍可在主列表和地址栏浏览。
    while let Some(current) = parent {
        parent = current.parent();
        places.push(current);
        if places.len() == 64 {
            break;
        }
    }
    places.reverse();
    let mut branches = Vec::new();
    for place in places {
        let entries = if place == location {
            Some(
                items
                    .iter()
                    .filter_map(|item| {
                        Some(Folder {
                            location: item.target.clone()?,
                            name: item.name.clone(),
                            icon: item.icon,
                        })
                    })
                    .collect(),
            )
        } else {
            folders(&place, &mounts).ok()
        };
        if let Some(folders) = entries {
            branches.push(Branch {
                location: place,
                folders,
            });
        }
    }
    Ok(Listing {
        location,
        items,
        truncated,
        free,
        branches,
    })
}
pub fn open(item: Item) -> io::Result<Child> {
    let mut command = if let Some(arguments) = item.launch {
        let mut command = Command::new(
            arguments
                .first()
                .ok_or_else(|| io::Error::other("空启动命令"))?,
        );
        command.args(&arguments[1..]);
        command
    } else {
        let path = item
            .path
            .ok_or_else(|| io::Error::other("项目没有可打开的路径"))?;
        desktop_entry::open(&path)?
    };
    command.env_remove("WAYLAND_SOCKET").spawn()
}
pub fn properties(item: Item) -> io::Result<Child> {
    let path = item
        .path
        .ok_or_else(|| io::Error::other("项目没有文件属性"))?;
    let mut command = Command::new("/bin/control-panel");
    command
        .arg("properties")
        .arg(path)
        .arg(item.icon.file_stem())
        .arg(item.kind)
        .env_remove("WAYLAND_SOCKET");
    if matches!(item.icon, Icon::Disk | Icon::OpticalDisc) {
        command.arg("drive");
    }
    command.spawn()
}
#[derive(Clone, Copy)]
pub enum Visit {
    Push,
    Refresh,
    History(usize),
}
#[derive(Clone)]
pub struct Clipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}
#[derive(Clone, Hash, PartialEq, Eq)]
enum ItemKey {
    Path(PathBuf),
    Virtual(Location, String, Option<Vec<String>>),
}
pub struct Node {
    pub parent: u64,
    pub location: Location,
    pub name: String,
    pub icon: Icon,
    pub children: Option<Vec<u64>>,
}
pub struct Catalogue {
    pub nodes: BTreeMap<u64, Node>,
    indices: HashMap<(u64, Location), u64>,
    next: u64,
}
impl Catalogue {
    pub fn new() -> Self {
        let mut catalogue = Self {
            nodes: BTreeMap::new(),
            indices: HashMap::new(),
            next: 1,
        };
        // 文件夹树与地址下拉的根是“桌面”；其下的系统文件夹随第一次读取桌面分支注册。
        catalogue.register(
            0,
            Folder {
                location: Location::Desktop,
                name: "桌面".into(),
                icon: Icon::Desktop,
            },
        );
        catalogue
    }
    fn register(&mut self, parent: u64, folder: Folder) -> Option<u64> {
        let key = (parent, folder.location.clone());
        if let Some(id) = self.indices.get(&key) {
            let node = self.nodes.get_mut(id).expect("catalogue index owns a node");
            node.name = folder.name;
            node.icon = folder.icon;
            return Some(*id);
        }
        let mut ancestor = parent;
        let mut depth = 0;
        while ancestor != 0 {
            ancestor = self.nodes.get(&ancestor)?.parent;
            depth += 1;
            if depth > 64 {
                return None;
            }
        }
        if self.nodes.len() >= 1_000_000 {
            return None;
        }
        let id = self.next;
        self.next += 1;
        self.indices.insert(key, id);
        self.nodes.insert(
            id,
            Node {
                parent,
                location: folder.location,
                name: folder.name,
                icon: folder.icon,
                children: None,
            },
        );
        Some(id)
    }
    fn remove(&mut self, id: u64) {
        if let Some(node) = self.nodes.remove(&id) {
            if let Some(children) = node.children {
                for child in children {
                    self.remove(child);
                }
            }
            self.indices.remove(&(node.parent, node.location));
        }
    }
    pub fn update(&mut self, branch: Branch) {
        // 同一真实路径可由磁盘树和“我的文档”等入口投影成不同节点，展开状态仍只在控件内保存。
        let parents: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| (node.location == branch.location).then_some(*id))
            .collect();
        for parent in parents {
            let wanted: std::collections::HashSet<_> = branch
                .folders
                .iter()
                .map(|folder| &folder.location)
                .collect();
            let removed: Vec<_> = self
                .nodes
                .get(&parent)
                .and_then(|node| node.children.as_ref())
                .into_iter()
                .flatten()
                .filter(|id| {
                    self.nodes
                        .get(id)
                        .is_some_and(|node| !wanted.contains(&node.location))
                })
                .copied()
                .collect();
            for id in removed {
                self.remove(id);
            }
            let children = branch
                .folders
                .iter()
                .filter_map(|folder| {
                    self.register(
                        parent,
                        Folder {
                            location: folder.location.clone(),
                            name: folder.name.clone(),
                            icon: folder.icon,
                        },
                    )
                })
                .collect();
            self.nodes
                .get_mut(&parent)
                .expect("catalogue parent")
                .children = Some(children);
        }
    }
    /// 节点自身及其全部祖先的标识，由近到远。
    pub fn ancestors(&self, mut id: u64) -> Vec<u64> {
        let mut chain = Vec::new();
        while let Some(node) = self.nodes.get(&id) {
            chain.push(id);
            id = node.parent;
        }
        chain
    }
    pub fn id(&self, location: &Location) -> Option<u64> {
        self.nodes
            .iter()
            .find_map(|(id, node)| (&node.location == location).then_some(*id))
    }
}
pub struct Browser {
    pub location: Location,
    pub items: Vec<(u64, Item)>,
    pub truncated: bool,
    pub free: Option<u64>,
    pub catalogue: Catalogue,
    pub clipboard: Option<Clipboard>,
    /// 排序列与是否降序；切换位置时若新位置没有该列则回到按名称升序。
    pub sort: (Field, bool),
    history: Vec<Location>,
    pub cursor: usize,
    next: u64,
}
impl Browser {
    pub fn new(location: Location) -> Self {
        Self {
            history: vec![location.clone()],
            location,
            items: Vec::new(),
            truncated: false,
            free: None,
            catalogue: Catalogue::new(),
            clipboard: None,
            sort: (Field::Name, false),
            cursor: 0,
            next: 1,
        }
    }
    pub fn publish(&mut self, listing: Listing, visit: Visit) {
        match visit {
            Visit::Push if listing.location != self.location => {
                self.history.truncate(self.cursor + 1);
                self.history.push(listing.location.clone());
                self.cursor += 1;
            }
            Visit::History(cursor) => self.cursor = cursor,
            _ => {}
        }
        let old_location = self.location.clone();
        self.location = listing.location;
        self.truncated = listing.truncated;
        self.free = listing.free;
        self.history[self.cursor] = self.location.clone();
        let key = |location: &Location, item: &Item| {
            item.path.clone().map(ItemKey::Path).unwrap_or_else(|| {
                ItemKey::Virtual(location.clone(), item.name.clone(), item.launch.clone())
            })
        };
        let mut previous: HashMap<_, _> = self
            .items
            .iter()
            .map(|(id, item)| (key(&old_location, item), *id))
            .collect();
        let mut identities = std::collections::HashSet::new();
        self.items = listing
            .items
            .into_iter()
            .filter_map(|item| {
                let key = key(&self.location, &item);
                if !identities.insert(key.clone()) {
                    return None;
                }
                let id = previous.remove(&key).unwrap_or_else(|| {
                    let id = self.next;
                    self.next += 1;
                    id
                });
                Some((id, item))
            })
            .collect();
        for branch in listing.branches {
            self.catalogue.update(branch);
        }
        if !self
            .location
            .columns()
            .iter()
            .any(|(field, ..)| *field == self.sort.0)
        {
            self.sort = (Field::Name, false);
        }
        self.sort_items();
    }
    /// 当前列表中的映射盘投影，用于拒绝后台读取期间已经过时的结果。
    pub(super) fn network_drives(&self) -> Vec<Item> {
        let mut drives: Vec<_> = self
            .items
            .iter()
            .filter(|(_, item)| item.drive.is_some())
            .map(|(_, item)| item.clone())
            .collect();
        drives.sort_by_key(|item| item.drive);
        drives
    }

    /// 更新“我的电脑”的映射盘，复用 publish 的稳定行身份与排序，不重新读取远程目录。
    pub(super) fn publish_network_drives(&mut self, drives: Vec<Item>) {
        let mut items: Vec<_> = self
            .items
            .iter()
            .filter(|(_, item)| item.drive.is_none())
            .map(|(_, item)| item.clone())
            .collect();
        items.extend(drives);
        let folders = items
            .iter()
            .filter_map(|item| {
                Some(Folder {
                    location: item.target.clone()?,
                    name: item.name.clone(),
                    icon: item.icon,
                })
            })
            .collect();
        self.publish(
            Listing {
                location: Location::MyComputer,
                items,
                truncated: self.truncated,
                free: self.free,
                branches: vec![Branch {
                    location: Location::MyComputer,
                    folders,
                }],
            },
            Visit::Refresh,
        );
    }

    pub fn sort_items(&mut self) {
        let (column, descending) = self.sort;
        self.items.sort_by(|(_, a), (_, b)| {
            a.group()
                .cmp(&b.group())
                .then_with(|| {
                    let order = a.compare(b, column);
                    if descending { order.reverse() } else { order }
                })
                .then_with(|| a.name.cmp(&b.name))
        });
    }
    pub fn item(&self, id: u64) -> Option<&Item> {
        self.items
            .iter()
            .find(|(row, _)| *row == id)
            .map(|(_, item)| item)
    }
    /// 旅行日志：（下标，标题），后退方向由近到远（`step` 为 -1）或前进方向由近到远（+1）。
    pub fn travel(&self, step: isize) -> Vec<(usize, String)> {
        let mut entries = Vec::new();
        let mut index = self.cursor as isize + step;
        while let Some(location) = usize::try_from(index)
            .ok()
            .and_then(|index| self.history.get(index))
        {
            entries.push((index as usize, location.title()));
            index += step;
        }
        entries
    }
    /// 全部历史位置：（下标，标题，是否当前）。
    pub fn journal(&self) -> Vec<(usize, String, bool)> {
        self.history
            .iter()
            .enumerate()
            .map(|(index, location)| (index, location.title(), index == self.cursor))
            .collect()
    }
    pub fn history(&self, offset: isize) -> Option<(usize, Location)> {
        let index = self.cursor.checked_add_signed(offset)?;
        Some((index, self.history.get(index)?.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refreshed_drive_updates_icon_without_replacing_its_subtree() {
        let mut catalogue = Catalogue::new();
        let drive = Location::Directory("/root/Network/Z".into());
        let folder = |icon| Folder {
            location: drive.clone(),
            name: "共享 (Z:)".into(),
            icon,
        };
        let id = catalogue.register(1, folder(Icon::OfflineDrive)).unwrap();
        catalogue.update(Branch {
            location: drive.clone(),
            folders: vec![Folder {
                location: Location::Directory("/root/Network/Z/教程".into()),
                name: "教程".into(),
                icon: Icon::Folder,
            }],
        });
        let children = catalogue.nodes[&id].children.clone();
        assert_eq!(catalogue.register(1, folder(Icon::NetworkDrive)), Some(id));
        assert!(matches!(catalogue.nodes[&id].icon, Icon::NetworkDrive));
        assert_eq!(catalogue.nodes[&id].children, children);
    }
}
