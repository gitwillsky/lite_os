//! 文件浏览的领域数据：目录快照、路径身份、导航历史和文件夹目录；不保存控件选择、展开或滚动。
use crate::{
    fileops,
    model::{Icon, Item, Location},
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
    pub branches: Vec<Branch>,
}
fn folders(location: &Location, mounts: &str) -> io::Result<Vec<Folder>> {
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
    if places.first() != Some(&Location::MyComputer) {
        places.insert(0, Location::MyComputer);
    }
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
        if item.icon == Icon::Application {
            Command::new(path)
        } else {
            desktop_entry::open_with(Path::new("/usr/share/applications"), &path)
                .ok_or_else(|| io::Error::other(format!("没有与 {} 关联的程序", item.name)))?
        }
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
    pub fn new(home: &Path) -> Self {
        let mut catalogue = Self {
            nodes: BTreeMap::new(),
            indices: HashMap::new(),
            next: 1,
        };
        for (location, name, icon) in [
            (Location::MyComputer, "我的电脑", Icon::MyComputer),
            (Location::NetworkPlaces, "网上邻居", Icon::NetworkPlaces),
            (
                Location::Directory(home.to_owned()),
                "我的文档",
                Icon::MyDocuments,
            ),
            (
                Location::Directory(fileops::trash_files(home)),
                "回收站",
                Icon::RecycleBin,
            ),
        ] {
            catalogue.register(
                0,
                Folder {
                    location,
                    name: name.into(),
                    icon,
                },
            );
        }
        catalogue
    }
    fn register(&mut self, parent: u64, folder: Folder) -> Option<u64> {
        let key = (parent, folder.location.clone());
        if let Some(id) = self.indices.get(&key) {
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
    pub catalogue: Catalogue,
    pub clipboard: Option<Clipboard>,
    pub sort: (u64, bool),
    history: Vec<Location>,
    pub cursor: usize,
    next: u64,
}
impl Browser {
    pub fn new(home: &Path, location: Location) -> Self {
        Self {
            history: vec![location.clone()],
            location,
            items: Vec::new(),
            truncated: false,
            catalogue: Catalogue::new(home),
            clipboard: None,
            sort: (1, false),
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
        self.sort_items();
    }
    pub fn sort_items(&mut self) {
        let (column, descending) = self.sort;
        self.items.sort_by(|(_, a), (_, b)| {
            b.target
                .is_some()
                .cmp(&a.target.is_some())
                .then_with(|| {
                    let order = match column {
                        2 => a.size.cmp(&b.size),
                        3 => a.kind.cmp(&b.kind),
                        4 => a.modified.cmp(&b.modified),
                        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    };
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
    pub fn history(&self, offset: isize) -> Option<(usize, Location)> {
        let index = self.cursor.checked_add_signed(offset)?;
        Some((index, self.history.get(index)?.clone()))
    }
}
