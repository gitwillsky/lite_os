//! “本地连接 属性”与“Internet 协议 (TCP/IP) 属性”：按 Windows 2000 netshell 对话框 16004 与 netcfgx 对话框 2323 还原。
//!
//! 草稿是 `/etc/network/interfaces` 中该接口的配置；“确定”后原子写回并让 network-service 重新配置接口。
//! 组件列表只有 Internet 协议 (TCP/IP)：系统没有 Microsoft 网络客户端、文件和打印机共享等组件，安装/卸载与
//! 网卡“配置”没有对应后端而禁用；“高级”TCP/IP 设置（多地址、WINS、IP 筛选）同样禁用。
use crate::{Activity, Apply, Page, Response, child, group, ip_address::IpAddress};
use liteos_ui::{
    Dialogs, Event, EventKind, Layout, PropertySheet, SheetStyle, ValidationError, Widget,
    WidgetKind as Kind, state,
};
use network_config::{CONFIG_PATH, Config, Interface, Method};
use std::{cell::RefCell, net::Ipv4Addr, path::Path, rc::Rc};

/// 组件描述（nettcpip.inf 的 MS_TCPIP.HelpText）。
const TCPIP_HELP: &str = "TCP/IP 是默认的广域网协议。它提供跨越多种互联网络的通讯。";

/// 后台读取的连接属性；界面构建不再执行配置文件与 sysfs I/O。
pub struct Data {
    saved: Interface,
    driver: String,
}
impl Data {
    pub fn read(interface: Option<String>) -> Result<Self, String> {
        let link = network_config::ethernet_links()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|link| {
                interface
                    .as_ref()
                    .is_none_or(|name| name == &link.interface)
            })
            .ok_or("没有网络连接")?;
        let saved = Config::load(Path::new(CONFIG_PATH))
            .map_err(|error| error.to_string())?
            .interface(&link.interface);
        Ok(Self {
            saved,
            driver: link.driver,
        })
    }
}

pub struct Connection {
    _controls: Vec<Widget>,
    components: Widget,
    properties: Widget,
    description: Widget,
    dialogs: Dialogs,
    window: liteos_ui::Window,
    saved: Interface,
    draft: Rc<RefCell<Interface>>,
}

impl Connection {
    pub fn build(sheet: &PropertySheet, data: Data, dialogs: Dialogs) -> liteos_ui::Result<Self> {
        let Data { saved, driver } = data;
        let page = sheet.page("常规")?;
        let mut controls = Vec::new();
        controls.push(child(&page, Kind::Label, "连接时使用:", Layout::default())?);
        // 网卡框：凹陷边框内是网卡图标与设备名（驱动名）。
        let adapter = child(
            &page,
            Kind::Row,
            "",
            Layout {
                height: 26,
                padding: 3,
                gap: 6,
                ..Default::default()
            },
        )?;
        adapter.set_state(state::CLIENT_EDGE)?;
        controls.push(child(
            &adapter,
            Kind::Image,
            "network-local",
            Layout {
                width: 16,
                height: 16,
                ..Default::default()
            },
        )?);
        controls.push(child(&adapter, Kind::Label, &driver, Layout::default())?);
        let configure_row = child(&page, Kind::Row, "", Layout::default())?;
        controls.push(child(
            &configure_row,
            Kind::Column,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?);
        let configure = child(
            &configure_row,
            Kind::Button,
            "配置(C)",
            Layout {
                width: 75,
                height: 21,
                ..Default::default()
            },
        )?;
        configure.set_state(state::DISABLED)?;
        controls.push(child(
            &page,
            Kind::Label,
            "此连接使用下列选定的组件(O):",
            Layout::default(),
        )?);
        let components = child(
            &page,
            Kind::List,
            "",
            Layout {
                height: 72,
                ..Default::default()
            },
        )?;
        components.set_rows(&[(1, "Internet 协议 (TCP/IP)")])?;
        components.select(Some(1))?;
        let buttons = child(&page, Kind::Row, "", Layout::default())?;
        let size = Layout {
            width: 100,
            height: 21,
            ..Default::default()
        };
        for title in ["安装(I)...", "卸载(U)"] {
            let button = child(&buttons, Kind::Button, title, size)?;
            button.set_state(state::DISABLED)?;
            controls.push(button);
        }
        let properties = child(&buttons, Kind::Button, "属性(R)", size)?;
        let description_group = group(&page, "描述")?;
        let description = child(
            &description_group,
            Kind::Label,
            TCPIP_HELP,
            Layout::default(),
        )?;
        description.set_state(state::WRAP)?;
        // 当前通知区域汇总显示网络状态，单接口开关尚未提供。
        let tray = child(
            &page,
            Kind::CheckBox,
            "连接后在任务栏中显示图标(W)",
            Layout::default(),
        )?;
        tray.set_state(state::DISABLED | state::CHECKED)?;
        controls.extend([
            adapter,
            configure_row,
            configure,
            buttons,
            description_group,
            tray,
        ]);
        controls.push(page);
        Ok(Self {
            _controls: controls,
            components,
            properties,
            description,
            dialogs,
            window: sheet.window().clone(),
            draft: Rc::new(RefCell::new(saved.clone())),
            saved,
        })
    }
}

impl Page for Connection {
    fn event(&mut self, event: &Event) -> liteos_ui::Result<Option<Response>> {
        let open = matches!(event.kind, EventKind::Activate { .. })
            && (event.node == self.properties.id() || event.node == self.components.id());
        if matches!(event.kind, EventKind::Select { .. }) && event.node == self.components.id() {
            let selected = self.components.selection()?.is_some();
            self.properties
                .set_state(if selected { 0 } else { state::DISABLED })?;
            self.description
                .set_text(if selected { TCPIP_HELP } else { "" })?;
        }
        if !open || self.components.selection()?.is_none() {
            return Ok(None);
        }
        let task: Activity = Box::pin(tcpip_dialog(
            self.dialogs.clone(),
            self.window.clone(),
            self.draft.clone(),
        ));
        Ok(Some(Response::Task(task)))
    }

    fn dirty(&self) -> liteos_ui::Result<bool> {
        Ok(*self.draft.borrow() != self.saved)
    }

    fn apply(&self) -> liteos_ui::Result<Apply> {
        let draft = self.draft.borrow().clone();
        Ok(Box::new(move || {
            let path = Path::new(CONFIG_PATH);
            let mut config = Config::load(path).map_err(|error| error.to_string())?;
            config.set(draft);
            config
                .store(path)
                .and_then(|()| network_config::reload_service())
                .map_err(|error| format!("无法应用网络设置：{error}"))
        }))
    }

    fn committed(&mut self) -> liteos_ui::Result<()> {
        self.saved = self.draft.borrow().clone();
        Ok(())
    }
}

/// TCP/IP 属性页的控件。
struct Tcpip {
    page: Widget,
    automatic: Widget,
    manual: Widget,
    address: IpAddress,
    netmask: IpAddress,
    gateway: IpAddress,
    automatic_dns: Widget,
    manual_dns: Widget,
    preferred: IpAddress,
    alternate: IpAddress,
    _controls: Vec<Widget>,
}

fn field_row(
    parent: &Widget,
    title: &str,
    controls: &mut Vec<Widget>,
) -> liteos_ui::Result<IpAddress> {
    let row = child(parent, Kind::Row, "", Layout::default())?;
    controls.push(child(
        &row,
        Kind::Label,
        title,
        Layout {
            width: 174,
            ..Default::default()
        },
    )?);
    let field = IpAddress::new(&row)?;
    controls.push(row);
    Ok(field)
}

impl Tcpip {
    fn build(sheet: &PropertySheet, draft: &Interface) -> liteos_ui::Result<Self> {
        let page = sheet.page("常规")?;
        let mut controls = Vec::new();
        let intro = child(
            &page,
            Kind::Label,
            "如果网络支持此功能，则可以获取自动指派的 IP 设置。否则，您需要从网络系统管理员处获得适当的 IP 设置。",
            Layout::default(),
        )?;
        intro.set_state(state::WRAP)?;
        let automatic = child(&page, Kind::Radio, "自动获得 IP 地址(O)", Layout::default())?;
        let address_group = group(&page, "")?;
        let manual = child(
            &address_group,
            Kind::Radio,
            "使用下面的 IP 地址(S):",
            Layout::default(),
        )?;
        let address = field_row(&address_group, "IP 地址(I):", &mut controls)?;
        let netmask = field_row(&address_group, "子网掩码(U):", &mut controls)?;
        let gateway = field_row(&address_group, "默认网关(D):", &mut controls)?;
        let automatic_dns = child(
            &page,
            Kind::Radio,
            "自动获得 DNS 服务器地址(B)",
            Layout::default(),
        )?;
        let dns_group = group(&page, "")?;
        let manual_dns = child(
            &dns_group,
            Kind::Radio,
            "使用下面的 DNS 服务器地址(E):",
            Layout::default(),
        )?;
        let preferred = field_row(&dns_group, "首选 DNS 服务器(P):", &mut controls)?;
        let alternate = field_row(&dns_group, "备用 DNS 服务器(A):", &mut controls)?;
        let advanced_row = child(&page, Kind::Row, "", Layout::default())?;
        controls.push(child(
            &advanced_row,
            Kind::Column,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?);
        let advanced = child(
            &advanced_row,
            Kind::Button,
            "高级(V)...",
            Layout {
                width: 90,
                height: 21,
                ..Default::default()
            },
        )?;
        advanced.set_state(state::DISABLED)?;
        controls.extend([intro, address_group, dns_group, advanced_row, advanced]);
        let tcpip = Self {
            page,
            automatic,
            manual,
            address,
            netmask,
            gateway,
            automatic_dns,
            manual_dns,
            preferred,
            alternate,
            _controls: controls,
        };
        if let Method::Static {
            address,
            netmask,
            gateway,
        } = draft.method
        {
            tcpip.address.set(Some(address))?;
            tcpip.netmask.set(Some(netmask))?;
            tcpip.gateway.set(gateway)?;
        }
        tcpip.preferred.set(draft.dns.first().copied())?;
        tcpip.alternate.set(draft.dns.get(1).copied())?;
        tcpip.choose(
            matches!(draft.method, Method::Static { .. }),
            !draft.dns.is_empty(),
        )?;
        Ok(tcpip)
    }

    /// 单选状态与字段可用性；手动 IP 时 DNS 必须手动（Windows 2000 的同一约束）。
    fn choose(&self, manual: bool, manual_dns: bool) -> liteos_ui::Result<()> {
        let manual_dns = manual || manual_dns;
        let checked = |on: bool| if on { state::CHECKED } else { 0 };
        self.automatic.set_state(checked(!manual))?;
        self.manual.set_state(checked(manual))?;
        self.automatic_dns
            .set_state(checked(!manual_dns) | if manual { state::DISABLED } else { 0 })?;
        self.manual_dns.set_state(checked(manual_dns))?;
        for field in [&self.address, &self.netmask, &self.gateway] {
            field.set_enabled(manual)?;
        }
        for field in [&self.preferred, &self.alternate] {
            field.set_enabled(manual_dns)?;
        }
        Ok(())
    }

    fn manual(&self) -> bool {
        self.manual.state().unwrap_or(0) & state::CHECKED != 0
    }

    fn manual_dns(&self) -> bool {
        self.manual_dns.state().unwrap_or(0) & state::CHECKED != 0
    }

    fn error(&self, control: Widget, message: &str) -> ValidationError {
        ValidationError {
            page: Some(self.page.clone()),
            control: Some(control),
            message: message.into(),
        }
    }

    /// 校验并生成新的接口配置（netcfgx 23003–23066 的检查）。
    fn draft(&self, previous: &Interface) -> Result<Interface, ValidationError> {
        let read = |field: &IpAddress| {
            field
                .value()
                .map_err(|control| self.error(control, "无效的 IP 或子网掩码地址。"))
        };
        let method = if self.manual() {
            let address = read(&self.address)?.ok_or_else(|| {
                self.error(
                    self.manual.clone(),
                    "适配器需要至少一个 IP 地址 , 请输入一个。",
                )
            })?;
            let netmask = read(&self.netmask)?.ok_or_else(|| {
                self.error(
                    self.manual.clone(),
                    "输入了丢失子网掩码的地址。请添加子网掩码。",
                )
            })?;
            let mask = u32::from(netmask);
            if mask == 0 || mask.leading_ones() + mask.trailing_zeros() != 32 {
                return Err(self.error(
                    self.manual.clone(),
                    "您输入一个无效的子网掩码。子网掩码必须是相邻的。请输入一个有效的掩码。",
                ));
            }
            let first = address.octets()[0];
            if first == 127 {
                return Err(self.error(
                    self.manual.clone(),
                    "以 127 起头的 IP 地址无效，因为它们保留用作环回地址。请再 1 和 223 之间指定一些其他有效值。",
                ));
            }
            if !(1..=223).contains(&first) {
                return Err(self.error(
                    self.manual.clone(),
                    &format!("{first} 不是一个主机 IP 地址第一个字段的有效项目。请指定一个在 1 和 223 之间的值。"),
                ));
            }
            Method::Static {
                address,
                netmask,
                gateway: read(&self.gateway)?,
            }
        } else {
            Method::Dhcp
        };
        let dns: Vec<Ipv4Addr> = if self.manual_dns() {
            [read(&self.preferred)?, read(&self.alternate)?]
                .into_iter()
                .flatten()
                .collect()
        } else {
            Vec::new()
        };
        let mut next = previous.clone();
        next.method = method;
        next.dns = dns;
        Ok(next)
    }

    fn event(&self, event: &Event) -> liteos_ui::Result<()> {
        for field in [
            &self.address,
            &self.netmask,
            &self.gateway,
            &self.preferred,
            &self.alternate,
        ] {
            if field.owns(event) {
                return field.event(event);
            }
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        let node = event.node;
        if node == self.automatic.id() {
            self.choose(false, self.manual_dns())
        } else if node == self.manual.id() {
            self.choose(true, true)
        } else if node == self.automatic_dns.id() {
            self.choose(self.manual(), false)
        } else if node == self.manual_dns.id() {
            self.choose(self.manual(), true)
        } else {
            Ok(())
        }
    }
}

/// 模态的“Internet 协议 (TCP/IP) 属性”：确定时把校验后的设置写入连接属性的草稿（真正生效在连接属性的“确定”）。
async fn tcpip_dialog(dialogs: Dialogs, parent: liteos_ui::Window, draft: Rc<RefCell<Interface>>) {
    let Ok((sheet, mut events)) = dialogs.property_sheet(
        &parent,
        "Internet 协议 (TCP/IP) 属性",
        (400, 400),
        SheetStyle::NoApply,
    ) else {
        return;
    };
    let current = draft.borrow().clone();
    let Ok(tcpip) = Tcpip::build(&sheet, &current) else {
        return;
    };
    let mut pending = None;
    while let Ok(Some(event)) = events.next().await {
        match event.kind {
            EventKind::Close => return,
            EventKind::Validate { token } => {
                let result = tcpip.draft(&current);
                pending = result.as_ref().ok().cloned();
                let _ = sheet.validated(token, result.map(drop));
            }
            EventKind::Apply { token } => {
                if let Some(next) = pending.take() {
                    *draft.borrow_mut() = next;
                }
                let _ = sheet.applied(token, Ok(()));
            }
            _ => {
                let _ = tcpip.event(&event);
            }
        }
    }
}
