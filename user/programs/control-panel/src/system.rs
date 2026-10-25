//! 系统信息在后台读取；三页仅声明标准控件和设备管理器启动动作。
use crate::{Page, child, group, image, label, pair};
use liteos_ui::{Event, EventKind, Layout, PropertySheet, Widget, WidgetKind as Kind};
use std::fs;
pub struct Data {
    groups: Vec<(&'static str, Vec<String>)>,
    host: String,
}

fn proc_field(path: &str, key: &str) -> Option<String> {
    fs::read_to_string(path).ok()?.lines().find_map(|line| {
        line.strip_prefix(key)?
            .trim_start_matches([':', ' ', '\t'])
            .to_owned()
            .into()
    })
}

/// `1,234,567` 样式的千分位，与 Windows 2000 的内存显示一致。
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

impl Data {
    pub fn read() -> Self {
        let release = fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|v| v.trim().to_owned())
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "未知".into());
        let host = fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h.trim().to_owned())
            .unwrap_or_else(|_| "未知".into());
        let user = std::env::var("USER")
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "root".into());
        let processors = std::thread::available_parallelism().map_or(1, usize::from);
        let memory = proc_field("/proc/meminfo", "MemTotal")
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .map_or_else(|| "未知".into(), |kib| format!("{} KB RAM", grouped(kib)));
        Self {
            groups: vec![
                ("系统：", vec!["LiteOS 2000 Professional".into(), release]),
                ("注册到：", vec![user]),
                (
                    "计算机：",
                    vec![format!("{processors} 个逻辑处理器"), memory],
                ),
            ],
            host,
        }
    }
}

pub struct System {
    _controls: Vec<Widget>,
    devices: Widget,
}
impl System {
    pub fn build(sheet: &PropertySheet, data: Data) -> liteos_ui::Result<Self> {
        let general = sheet.page("常规")?;
        let mut controls = vec![image(&general, "liteos-logo")?];
        for (heading, values) in data.groups {
            let section = group(&general, heading)?;
            for value in values {
                controls.push(label(&section, &value)?);
            }
            controls.push(section);
        }
        let network = sheet.page("网络标识")?;
        controls.push(image(&network, "my-computer")?);
        controls.push(label(
            &network,
            "LiteOS 使用以下信息在网络上标识您的计算机。",
        )?);
        let (a, b) = pair(&network, "计算机全名：", &data.host)?;
        controls.extend([a, b]);
        let hardware = sheet.page("硬件")?;
        let section = group(&hardware, "设备管理器")?;
        controls.push(label(
            &section,
            "设备管理器列出系统里能确认的硬件，并显示每个设备的属性。",
        )?);
        let devices = child(
            &section,
            Kind::Button,
            "设备管理器(D)",
            Layout {
                width: 140,
                height: 26,
                ..Default::default()
            },
        )?;
        controls.extend([general, network, hardware, section]);
        Ok(Self {
            _controls: controls,
            devices,
        })
    }
}
impl Page for System {
    fn event(&mut self, event: &Event) -> liteos_ui::Result<Option<Vec<String>>> {
        Ok(
            (event.node == self.devices.id() && matches!(event.kind, EventKind::Activate { .. }))
                .then(|| vec!["/bin/devmgmt".into()]),
        )
    }
}
