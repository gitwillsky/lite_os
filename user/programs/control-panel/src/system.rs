//! 系统属性。主 DNS 后缀在网络标识中编辑，由 router_config 持久化。
use crate::{Apply, Page, Response, child, group, image, label, pair};
use liteos_ui::{
    Dialogs, Event, EventKind, Layout, PropertySheet, SheetStyle, ValidationError, Widget,
    WidgetKind as Kind, Window, state,
};
use std::{cell::RefCell, fs, rc::Rc};
pub struct Data {
    groups: Vec<(&'static str, Vec<String>)>,
    host: String,
    primary_suffix: Option<String>,
}

fn full_name(host: &str, suffix: &str) -> String {
    if suffix.is_empty() || host.is_empty() {
        return host.into();
    }
    let name = host.split_once('.').map_or(host, |(name, _)| name);
    format!("{name}.{suffix}")
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
        let identity = linux_uapi::uts::read().ok();
        let release = identity.as_ref().map_or_else(
            || "未知".into(),
            |identity| identity.release.to_string_lossy().into_owned(),
        );
        let host = identity.as_ref().map_or_else(
            || "未知".into(),
            |identity| identity.hostname.to_string_lossy().into_owned(),
        );
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
            primary_suffix: network_config::snapshot()
                .ok()
                .map(|snapshot| snapshot.primary_dns_suffix),
        }
    }
}

pub struct System {
    _controls: Vec<Widget>,
    devices: Widget,
    network_properties: Widget,
    full_name: Widget,
    dialogs: Dialogs,
    window: Window,
    host: String,
    saved: Option<String>,
    draft: Rc<RefCell<String>>,
}
impl System {
    pub fn build(sheet: &PropertySheet, data: Data, dialogs: Dialogs) -> liteos_ui::Result<Self> {
        let general = sheet.page("常规")?;
        let mut controls = vec![image(&general, "windows-logo")?];
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
        let suffix = data.primary_suffix.as_deref().unwrap_or_default();
        let (heading, full_name) = pair(&network, "计算机全名：", &full_name(&data.host, suffix))?;
        full_name.set_state(state::WRAP)?;
        let network_properties = child(
            &network,
            Kind::Button,
            "属性(R)",
            Layout {
                width: 80,
                height: 24,
                ..Default::default()
            },
        )?;
        if data.primary_suffix.is_none() {
            network_properties.set_state(state::DISABLED)?;
        }
        controls.push(heading);
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
            network_properties,
            full_name,
            dialogs,
            window: sheet.window().clone(),
            host: data.host,
            draft: Rc::new(RefCell::new(suffix.into())),
            saved: data.primary_suffix,
        })
    }
}
impl Page for System {
    fn event(&mut self, event: &Event) -> liteos_ui::Result<Option<Response>> {
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(None);
        }
        if event.node == self.devices.id() {
            return Ok(Some(Response::Launch(vec!["/bin/devmgmt".into()])));
        }
        if event.node == self.network_properties.id() {
            let dialogs = self.dialogs.clone();
            let window = self.window.clone();
            let host = self.host.clone();
            let full = self.full_name.clone();
            let draft = self.draft.clone();
            let initial = draft.borrow().clone();
            return Ok(Some(Response::Task(Box::pin(async move {
                if let Ok(Some(suffix)) =
                    edit_identification(&dialogs, &window, &host, initial).await
                {
                    let _ = full.set_text(&full_name(&host, &suffix));
                    *draft.borrow_mut() = suffix;
                }
            }))));
        }
        Ok(None)
    }

    fn dirty(&self) -> liteos_ui::Result<bool> {
        Ok(self
            .saved
            .as_ref()
            .is_some_and(|saved| saved != &*self.draft.borrow()))
    }

    fn apply(&self) -> liteos_ui::Result<Apply> {
        let suffix = self.dirty()?.then(|| self.draft.borrow().clone());
        Ok(Box::new(move || {
            if let Some(suffix) = suffix {
                network_config::set_primary_dns_suffix(suffix)
                    .map_err(|error| format!("无法保存主 DNS 后缀：{error}"))?;
            }
            Ok(())
        }))
    }

    fn committed(&mut self) -> liteos_ui::Result<()> {
        if self.saved.is_some() {
            self.saved = Some(self.draft.borrow().clone());
        }
        Ok(())
    }
}

async fn edit_identification(
    dialogs: &Dialogs,
    parent: &Window,
    host: &str,
    mut suffix: String,
) -> liteos_ui::Result<Option<String>> {
    let (sheet, mut events) =
        dialogs.property_sheet(parent, "标识更改", (386, 190), SheetStyle::NoApply)?;
    let page = sheet.page("网络标识")?;
    let (_host_label, _host) = pair(&page, "计算机名(C)：", host)?;
    let (_full_label, full) = pair(&page, "计算机全名：", &full_name(host, &suffix))?;
    full.set_state(state::WRAP)?;
    let more = child(
        &page,
        Kind::Button,
        "其它(M)...",
        Layout {
            width: 80,
            height: 24,
            ..Default::default()
        },
    )?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close => break,
            EventKind::Validate { token } => sheet.validated(token, Ok(()))?,
            EventKind::Apply { token } => {
                sheet.applied(token, Ok(()))?;
                return Ok(Some(suffix));
            }
            EventKind::Activate { .. } if event.node == more.id() => {
                if let Some(next) = edit_primary_suffix(dialogs, sheet.window(), &suffix).await? {
                    suffix = next;
                    full.set_text(&full_name(host, &suffix))?;
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

async fn edit_primary_suffix(
    dialogs: &Dialogs,
    parent: &Window,
    suffix: &str,
) -> liteos_ui::Result<Option<String>> {
    let (sheet, mut events) = dialogs.property_sheet(
        parent,
        "DNS 后缀和 NetBIOS 计算机名",
        (345, 122),
        SheetStyle::NoApply,
    )?;
    let page = sheet.page("DNS 后缀")?;
    let _label = label(&page, "主 DNS 后缀(P):")?;
    let field = child(&page, Kind::Edit, suffix, Layout::default())?;
    field.focus()?;
    let mut pending = None;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close => break,
            EventKind::Validate { token } => {
                let text = field.text()?;
                pending = network_config::normalize_dns_suffix(&text).map(str::to_owned);
                sheet.validated(
                    token,
                    pending.as_ref().map(|_| ()).ok_or_else(|| ValidationError {
                        page: Some(page.clone()),
                        control: Some(field.clone()),
                        message: "DNS 后缀无效。请输入以句点分隔的 DNS 域名。".into(),
                    }),
                )?;
            }
            EventKind::Apply { token } => {
                sheet.applied(token, Ok(()))?;
                return Ok(pending.take());
            }
            _ => {}
        }
    }
    Ok(None)
}
