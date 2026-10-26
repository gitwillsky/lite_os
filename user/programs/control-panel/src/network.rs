//! “本地连接 状态”：按 Windows 2000 netshell 对话框 1801（常规页）还原。
//! 连接状态取 sysfs 与 network-service，活动计数在有界后台池按 `/proc/net/dev` 周期采样；
//! “属性”打开“本地连接 属性”，“禁用”写回接口配置并让 network-service 重新配置后关闭本窗口。
use crate::{Activity, Page, Response, child, group};
use liteos_ui::{
    Background, Clock, Event, EventKind, Layout, PropertySheet, Widget, WidgetKind as Kind, state,
};
use std::{fs, time::Duration};

/// 接口 `name` 的（收到字节，发出字节，收到错误，发出错误），取自 `/proc/net/dev`。
fn counters(name: &str) -> Option<[u64; 4]> {
    let text = fs::read_to_string("/proc/net/dev").ok()?;
    text.lines().skip(2).find_map(|line| {
        let (interface, rest) = line.split_once(':')?;
        if interface.trim() != name {
            return None;
        }
        let fields: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|value| value.parse().ok())
            .collect();
        Some([
            *fields.first()?,
            *fields.get(8)?,
            *fields.get(2)?,
            *fields.get(10)?,
        ])
    })
}

/// 连接已建立的秒数：network-service 按当前配置启用接口的时刻到现在（/proc 的进程启动时刻与 uptime）。
fn connected_seconds() -> Option<u64> {
    let pid = fs::read_to_string("/run/network-service.pid").ok()?;
    let stat = fs::read_to_string(format!("/proc/{}/stat", pid.trim())).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    // stat 第 22 个字段（`)` 之后第 20 个）是启动时刻，单位为时钟滴答（USER_HZ = 100）。
    let start: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
    let uptime: f64 = fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some((uptime as u64).saturating_sub(start / 100))
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
    duration: Widget,
    bytes: [Widget; 2],
    errors: [Widget; 2],
    properties: Widget,
    disable: Widget,
}

impl Network {
    pub fn build(sheet: &PropertySheet, data: Data) -> liteos_ui::Result<Self> {
        let general = sheet.page("常规")?;
        let mut controls = Vec::new();
        // “连接”分组：状态、持续时间、速度（netshell 1040–1050）。
        let connection = group(&general, "连接")?;
        let status = match data.0.state {
            network_config::LinkState::Disabled => "禁用",
            network_config::LinkState::Connected => "已连接上",
            network_config::LinkState::Disconnected => "网络电缆被拔出",
        };
        let mut duration = None;
        // 网络设备不报告链路速率（Linux 的 speed 属性在 virtio 上同样未知）。
        for (title, value) in [
            ("状态:", status),
            ("持续时间:", "00:00:00"),
            ("速度:", "未知"),
        ] {
            let row = child(&connection, Kind::Row, "", Layout::default())?;
            controls.push(label(&row, title, width(84))?);
            let value = label(&row, value, grow())?;
            if title == "持续时间:" {
                duration = Some(value.clone());
            }
            controls.extend([row, value]);
        }
        // “活动”分组：“发送 ◆ 收到”表头，下面是字节与错误计数（左列发送，右列收到）。
        let activity = group(&general, "活动")?;
        let header = child(&activity, Kind::Row, "", Layout::default())?;
        controls.push(label(&header, "", width(60))?);
        let sent = label(&header, "发送", grow())?;
        sent.set_state(state::ALIGN_RIGHT)?;
        let icon = child(
            &header,
            Kind::Image,
            "network-local",
            Layout {
                width: 32,
                height: 32,
                ..Default::default()
            },
        )?;
        let received = label(&header, "收到", grow())?;
        controls.extend([header, sent, icon, received]);
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
        if data.0.state == network_config::LinkState::Disabled {
            disable.set_state(state::DISABLED)?;
        }
        controls.extend([general, connection, activity, buttons]);
        let [bytes, errors] = <[[Widget; 2]; 2]>::try_from(counters)
            .ok()
            .expect("two counter rows");
        Ok(Self {
            _controls: controls,
            interface: data.0.interface,
            duration: duration.expect("duration row"),
            bytes,
            errors,
            properties,
            disable,
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
            // “禁用”：去掉接口的 auto 并让 network-service 关闭它，然后关闭状态窗口（与 Windows 2000 相同）。
            network_config::set_enabled(&self.interface, false)
                .map_err(|error| liteos_ui::Error::invalid(format!("无法禁用连接：{error}")))?;
            return Ok(Some(Response::Close));
        }
        Ok(None)
    }

    fn activity(&self, background: Background, clock: Clock) -> Option<Activity> {
        let (duration, bytes, errors) = (
            self.duration.clone(),
            self.bytes.clone(),
            self.errors.clone(),
        );
        let interface = self.interface.clone();
        Some(Box::pin(async move {
            loop {
                let name = interface.clone();
                let Ok(work) = background.run(move || (counters(&name), connected_seconds()))
                else {
                    break;
                };
                let (counts, seconds) = work.await;
                let [received, sent, received_errors, sent_errors] = counts.unwrap_or([0; 4]);
                let seconds = seconds.unwrap_or(0);
                let text = format!(
                    "{:02}:{:02}:{:02}",
                    seconds / 3600,
                    seconds / 60 % 60,
                    seconds % 60
                );
                let updated = duration.set_text(&text).is_ok()
                    && bytes[0].set_text(&grouped(sent)).is_ok()
                    && bytes[1].set_text(&grouped(received)).is_ok()
                    && errors[0].set_text(&grouped(sent_errors)).is_ok()
                    && errors[1].set_text(&grouped(received_errors)).is_ok();
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
