//! 设备清单：从系统里能确认的来源读出各类硬件；没有统一的内核设备枚举，所以只覆盖这些来源。
use linux_uapi::input::InputDevice;
use std::{fs, path::Path};

/// 一个设备：显示名与"属性"里的（项目，值）。
pub struct Device {
    pub name: String,
    pub details: Vec<(&'static str, String)>,
}

/// 一类设备。
pub struct Category {
    pub name: &'static str,
    pub devices: Vec<Device>,
}

fn device(name: impl Into<String>, details: Vec<(&'static str, String)>) -> Device {
    Device {
        name: name.into(),
        details,
    }
}

/// 读取全部类别，空类别不返回；类别顺序与 Windows 2000 的设备管理器一致（按名称）。
pub fn scan() -> Vec<Category> {
    let mut categories = vec![
        Category {
            name: "处理器",
            devices: processors(),
        },
        Category {
            name: "磁盘驱动器",
            devices: disks(),
        },
        Category {
            name: "键盘",
            devices: Vec::new(),
        },
        Category {
            name: "声音、视频和游戏控制器",
            devices: sound(),
        },
        Category {
            name: "网络适配器",
            devices: network(),
        },
        Category {
            name: "显示卡",
            devices: display(),
        },
        Category {
            name: "鼠标和其他指针设备",
            devices: Vec::new(),
        },
    ];
    // 输入设备按名字分成键盘与指针两类。
    for (name, path) in input_devices() {
        let is_keyboard = name.to_lowercase().contains("key");
        let target = if is_keyboard {
            "键盘"
        } else {
            "鼠标和其他指针设备"
        };
        if let Some(category) = categories.iter_mut().find(|c| c.name == target) {
            category.devices.push(device(
                name,
                vec![("设备类型", target.to_owned()), ("设备节点", path)],
            ));
        }
    }
    categories.retain(|category| !category.devices.is_empty());
    categories.sort_by(|a, b| a.name.cmp(b.name));
    categories
}

fn processors() -> Vec<Device> {
    let text = fs::read_to_string("/proc/stat").unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            line.split_whitespace()
                .next()?
                .strip_prefix("cpu")?
                .parse::<usize>()
                .ok()
        })
        .map(|index| {
            device(
                format!("{} 逻辑处理器 {index}", std::env::consts::ARCH),
                vec![
                    ("设备类型", "处理器".to_owned()),
                    ("架构", std::env::consts::ARCH.to_owned()),
                    ("编号", index.to_string()),
                ],
            )
        })
        .collect()
}

fn disks() -> Vec<Device> {
    let Ok(entries) = fs::read_dir("/sys/block") else {
        return Vec::new();
    };
    let mut disks: Vec<Device> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("loop") || name.starts_with("ram") {
                return None;
            }
            let sectors: u64 = fs::read_to_string(entry.path().join("size"))
                .ok()?
                .trim()
                .parse()
                .ok()?;
            Some(device(
                format!("磁盘 {name}"),
                vec![
                    ("设备类型", "磁盘驱动器".to_owned()),
                    ("设备节点", format!("/dev/{name}")),
                    ("容量", format!("{} MB", sectors * 512 / (1024 * 1024))),
                ],
            ))
        })
        .collect();
    disks.sort_by(|a, b| a.name.cmp(&b.name));
    disks
}

fn display() -> Vec<Device> {
    let Ok(entries) = fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut cards: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("card"))
        .collect();
    cards.sort();
    cards
        .into_iter()
        .map(|name| {
            device(
                format!("DRM 显示适配器 ({name})"),
                vec![
                    ("设备类型", "显示卡".to_owned()),
                    ("设备节点", format!("/dev/dri/{name}")),
                ],
            )
        })
        .collect()
}

fn sound() -> Vec<Device> {
    let node = Path::new("/dev/snd/pcmC0D0p");
    if !node.exists() {
        return Vec::new();
    }
    vec![device(
        "LiteOS Playback",
        vec![
            ("设备类型", "声音设备".to_owned()),
            ("设备节点", node.display().to_string()),
            ("功能", "播放（没有录音子流）".to_owned()),
        ],
    )]
}

fn network() -> Vec<Device> {
    let text = fs::read_to_string("/proc/net/dev").unwrap_or_default();
    text.lines()
        .skip(2)
        .filter_map(|line| Some(line.split_once(':')?.0.trim().to_owned()))
        .filter(|name| name != "lo")
        .map(|name| {
            device(
                format!("网络适配器 ({name})"),
                vec![
                    ("设备类型", "网络适配器".to_owned()),
                    ("接口名", name),
                    ("地址获取", "DHCP（network-service）".to_owned()),
                ],
            )
        })
        .collect()
}

/// `/dev/input/event*` 的（设备名，路径）；读不到名字的用节点名。
fn input_devices() -> Vec<(String, String)> {
    let Ok(entries) = fs::read_dir("/dev/input") else {
        return Vec::new();
    };
    let mut devices: Vec<(String, String)> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("event"))
        .map(|entry| {
            let path = entry.path();
            let name = InputDevice::open(&path)
                .and_then(|device| device.name())
                .unwrap_or_else(|_| entry.file_name().to_string_lossy().into_owned());
            (name, path.display().to_string())
        })
        .collect();
    devices.sort();
    devices
}
