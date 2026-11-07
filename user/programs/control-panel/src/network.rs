//! “本地连接 状态”：按 Windows 2000 netshell 对话框 1801（常规页）还原。
//! 连接状态取 sysfs 与网络设置（netcfg），活动计数在有界后台池按 `/proc/net/dev` 周期采样；
//! “详细信息”使用同一次 netcfg 快照呈现当前生效地址。
//! “属性”打开“本地连接 属性”，“禁用”经网络设置关闭接口后关闭本窗口。
use crate::{Activity, Page, Response, child, group, pair};
use liteos_ui::{
    Background, Clock, Event, EventKind, Layout, PropertySheet, Widget, WidgetKind as Kind, state,
};
use network_config::LinkState;
use std::{borrow::Cow, fs, net::Ipv4Addr, time::Duration};

const ADDRESS_FIELDS: [&str; 10] = [
    "物理地址：",
    "地址状态：",
    "IP 地址：",
    "子网掩码：",
    "默认网关：",
    "DNS 服务器：",
    "DNS 后缀：",
    "DHCP 服务器：",
    "获得租约：",
    "租约过期：",
];

fn address(value: Ipv4Addr) -> Cow<'static, str> {
    if value.is_unspecified() {
        "—".into()
    } else {
        value.to_string().into()
    }
}

/// 接口 `name` 的（收到字节，发出字节，收到错误，发出错误），取自 `/proc/net/dev`。
fn counters(name: &str) -> Option<[u64; 4]> {
    let text = fs::read_to_string("/proc/net/dev").ok()?;
    text.lines().skip(2).find_map(|line| {
        let (interface, rest) = line.split_once(':')?;
        if interface.trim() != name {
            return None;
        }
        let mut values = [0; 4];
        for (index, field) in rest.split_whitespace().enumerate() {
            match index {
                0 => values[0] = field.parse().ok()?,
                8 => values[1] = field.parse().ok()?,
                2 => values[2] = field.parse().ok()?,
                10 => {
                    values[3] = field.parse().ok()?;
                    return Some(values);
                }
                _ => {}
            }
        }
        None
    })
}

/// 一次后台读取。物理链路与 IP 配置时长分别取各自的 owner。
struct Sample {
    state: Option<LinkState>,
    speed: Option<u32>,
    seconds: Option<u32>,
    counts: Option<[u64; 4]>,
    addresses: [Cow<'static, str>; ADDRESS_FIELDS.len()],
}

impl Sample {
    fn read(interface: &str) -> Self {
        let settings = network_config::snapshot().ok();
        let state = network_config::link_state(interface, settings.as_ref()).ok();
        let configured = settings.as_ref().and_then(|settings| {
            settings
                .interfaces
                .iter()
                .find(|state| state.name == interface.as_bytes())
        });
        let connected = state == Some(LinkState::Connected);
        let seconds = configured
            .filter(|state| connected && !state.address.is_unspecified())
            .map(|state| state.connected_seconds);
        let speed = connected
            .then(|| {
                let text = fs::read_to_string(format!("/sys/class/net/{interface}/speed")).ok()?;
                text.trim().parse().ok()
            })
            .flatten();
        let mut addresses = std::array::from_fn(|_| Cow::Borrowed("—"));
        let [
            mac,
            allocation,
            ip,
            mask,
            gateway,
            dns,
            suffix,
            dhcp,
            obtained,
            expires,
        ] = &mut addresses;
        if let Some(config) = configured {
            *mac = network_config::format_mac(config.mac).into();
        }
        *allocation = match (state, configured) {
            (Some(LinkState::Connected), Some(config)) if config.address.is_unspecified() => {
                "尚未获得 IPv4 地址"
            }
            (Some(LinkState::Connected), Some(config)) => {
                *ip = config.address.to_string().into();
                *mask = network_config::prefix_mask(config.prefix_length)
                    .to_string()
                    .into();
                *gateway = address(config.gateway);
                if !config.dns.is_empty() {
                    *dns = config
                        .dns
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                        .into();
                }
                if !config.dns_suffix.is_empty() {
                    *suffix = config.dns_suffix.clone().into();
                }
                if let Some(lease) = config.lease {
                    *dhcp = address(lease.server);
                    *obtained = network_config::format_lease_time(lease.obtained_at).into();
                    *expires = lease.expires_at.map_or_else(
                        || Cow::Borrowed("无限期"),
                        |unix| network_config::format_lease_time(unix).into(),
                    );
                    "由 DHCP 分配"
                } else {
                    "已配置 IPv4 地址"
                }
            }
            (Some(LinkState::Disconnected), _) => "网络电缆被拔出",
            (Some(LinkState::Disabled), _) => "已禁用",
            _ => "状态不可用",
        }
        .into();
        Self {
            state,
            speed,
            seconds,
            counts: counters(interface),
            addresses,
        }
    }
}

fn presentation(state: Option<LinkState>) -> (&'static str, &'static str) {
    match state {
        Some(LinkState::Connected) => ("已连接上", "network-local"),
        Some(LinkState::Disabled) => ("禁用", "network-disabled"),
        Some(LinkState::Disconnected) => ("网络电缆被拔出", "network-disconnected"),
        None => ("连接不可用", "network-disconnected"),
    }
}

pub struct Data(network_config::Link);
impl Data {
    /// 从共享链路快照选择指定接口，缺省为第一个以太网接口；读取失败保留诊断。
    pub fn read(interface: Option<String>) -> Result<Self, String> {
        network_config::ethernet_links()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|link| {
                interface
                    .as_ref()
                    .is_none_or(|name| name == &link.interface)
            })
            .map(Self)
            .ok_or_else(|| "没有网络连接".into())
    }
}

/// 带千分位的整数（简体中文区域设置）。
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut text = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            text.push(',');
        }
        text.push(ch);
    }
    text
}

fn label(parent: &Widget, text: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    child(parent, Kind::Label, text, layout)
}

fn width(width: i32) -> Layout {
    Layout {
        width,
        ..Default::default()
    }
}

fn grow() -> Layout {
    Layout {
        grow: 1,
        ..Default::default()
    }
}

pub struct Network {
    _controls: Vec<Widget>,
    interface: String,
    initial: LinkState,
    status: Widget,
    duration: Widget,
    speed: Widget,
    icon: Widget,
    bytes: [Widget; 2],
    errors: [Widget; 2],
    properties: Widget,
    disable: Widget,
    addresses: Vec<Widget>,
}

impl Network {
    pub fn build(sheet: &PropertySheet, data: Data) -> liteos_ui::Result<Self> {
        sheet.window().set_title(&format!("{} 状态", data.0.name))?;
        let general = sheet.page("常规")?;
        let mut controls = Vec::new();
        // “连接”分组：状态、持续时间、速度（netshell 1040–1050）。
        let connection = group(&general, "连接")?;
        let (initial_status, initial_icon) = presentation(Some(data.0.state));
        let mut fields = Vec::new();
        for (title, value) in [
            ("状态:", initial_status),
            ("持续时间:", "00:00:00"),
            ("速度:", "未知"),
        ] {
            let row = child(&connection, Kind::Row, "", Layout::default())?;
            controls.push(label(&row, title, width(84))?);
            fields.push(label(&row, value, grow())?);
            controls.push(row);
        }
        let [status, duration, speed] = <[Widget; 3]>::try_from(fields)
            .ok()
            .expect("three connection fields");
        // “活动”分组：“发送 ◆ 收到”表头，下面是字节与错误计数（左列发送，右列收到）。
        let activity = group(&general, "活动")?;
        let header = child(&activity, Kind::Row, "", Layout::default())?;
        controls.push(label(&header, "", width(60))?);
        let sent = label(&header, "发送", grow())?;
        sent.set_state(state::ALIGN_RIGHT)?;
        let icon = child(
            &header,
            Kind::Image,
            initial_icon,
            Layout {
                width: 32,
                height: 32,
                ..Default::default()
            },
        )?;
        let received = label(&header, "收到", grow())?;
        controls.extend([header, sent, received]);
        let mut counters = Vec::new();
        for title in ["字节:", "错误:"] {
            let line = child(&activity, Kind::Row, "", Layout::default())?;
            controls.push(label(&line, title, width(60))?);
            let sent = label(&line, "0", grow())?;
            sent.set_state(state::ALIGN_RIGHT)?;
            controls.push(label(&line, "", width(32))?);
            let received = label(&line, "0", grow())?;
            controls.push(line);
            counters.push([sent, received]);
        }
        let buttons = child(&general, Kind::Row, "", Layout::default())?;
        let size = Layout {
            width: 75,
            height: 21,
            ..Default::default()
        };
        let properties = child(&buttons, Kind::Button, "属性(P)", size)?;
        let disable = child(&buttons, Kind::Button, "禁用(D)", size)?;
        if data.0.state == LinkState::Disabled {
            disable.set_state(state::DISABLED)?;
        }
        controls.extend([general, connection, activity, buttons]);
        let [bytes, errors] = <[[Widget; 2]; 2]>::try_from(counters)
            .ok()
            .expect("two counter rows");
        let details = sheet.page("详细信息")?;
        let mut addresses = Vec::new();
        for title in ADDRESS_FIELDS {
            let (heading, value) = pair(&details, title, "—")?;
            value.set_state(state::WRAP)?;
            addresses.push(value);
            controls.push(heading);
        }
        controls.push(details);
        Ok(Self {
            _controls: controls,
            interface: data.0.interface,
            initial: data.0.state,
            status,
            duration,
            speed,
            icon,
            bytes,
            errors,
            properties,
            disable,
            addresses,
        })
    }
}

impl Page for Network {
    fn event(&mut self, event: &Event) -> liteos_ui::Result<Option<Response>> {
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(None);
        }
        if event.node == self.properties.id() {
            return Ok(Some(Response::Launch(vec![
                "/bin/control-panel".into(),
                "connection".into(),
                self.interface.clone(),
            ])));
        }
        if event.node == self.disable.id() {
            // “禁用”要等网络设置服务确认；后台等待期间仍可处理窗口事件。
            let interface = self.interface.clone();
            return Ok(Some(Response::CloseAfter(Box::new(move || {
                network_config::set_enabled(&interface, false)
                    .map_err(|error| format!("无法禁用连接：{error}"))
            }))));
        }
        Ok(None)
    }

    fn activity(&self, background: Background, clock: Clock) -> Option<Activity> {
        let [status, duration, speed, icon, properties, disable] = [
            &self.status,
            &self.duration,
            &self.speed,
            &self.icon,
            &self.properties,
            &self.disable,
        ]
        .map(Widget::clone);
        let bytes = self.bytes.clone();
        let errors = self.errors.clone();
        let interface = self.interface.clone();
        let addresses = self.addresses.clone();
        let mut previous = Some(self.initial);
        let mut previous_addresses = None;
        Some(Box::pin(async move {
            loop {
                let name = interface.clone();
                let Ok(work) = background.run(move || Sample::read(&name)) else {
                    break;
                };
                let sample = work.await;
                if previous_addresses.as_ref() != Some(&sample.addresses) {
                    if addresses
                        .iter()
                        .zip(&sample.addresses)
                        .any(|(value, text)| value.set_text(text).is_err())
                    {
                        break;
                    }
                    previous_addresses = Some(sample.addresses);
                }
                if previous != sample.state {
                    let (text, artwork) = presentation(sample.state);
                    let disabled = if matches!(sample.state, None | Some(LinkState::Disabled)) {
                        state::DISABLED
                    } else {
                        0
                    };
                    let absent = if sample.state.is_none() {
                        state::DISABLED
                    } else {
                        0
                    };
                    if status.set_text(text).is_err()
                        || icon.set_text(artwork).is_err()
                        || disable.set_state(disabled).is_err()
                        || properties.set_state(absent).is_err()
                    {
                        break;
                    }
                    previous = sample.state;
                }
                let seconds = sample.seconds.unwrap_or(0);
                let elapsed = format!(
                    "{:02}:{:02}:{:02}",
                    seconds / 3600,
                    seconds / 60 % 60,
                    seconds % 60
                );
                let rate = sample
                    .speed
                    .map_or_else(|| "未知".into(), |speed| format!("{speed}.0 Mbps"));
                let count = |index: usize| {
                    sample
                        .counts
                        .map_or_else(|| "—".into(), |counts| grouped(counts[index]))
                };
                let updated = duration.set_text(&elapsed).is_ok()
                    && speed.set_text(&rate).is_ok()
                    && bytes[0].set_text(&count(1)).is_ok()
                    && bytes[1].set_text(&count(0)).is_ok()
                    && errors[0].set_text(&count(3)).is_ok()
                    && errors[1].set_text(&count(2)).is_ok();
                if !updated {
                    break;
                }
                let Ok(wait) = clock.sleep(Duration::from_millis(500)) else {
                    break;
                };
                wait.await;
            }
        }))
    }
}
