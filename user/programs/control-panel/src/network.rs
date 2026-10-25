//! 网络信息和周期采样在有界后台池读取，界面只更新标准标签。
use crate::{Activity, Page, group, label, pair};
use liteos_ui::{Background, Clock, PropertySheet, Widget};
use std::{fs, process::Command, time::Duration};
struct Address {
    ip: String,
    mask: String,
    gateway: String,
    dns: String,
}

pub struct Data {
    interface: Option<String>,
    address: Option<Address>,
}

/// `/proc/net/dev` 里第一个非回环接口及它的（收到字节，发出字节）。
fn counters() -> Option<(String, u64, u64)> {
    let text = fs::read_to_string("/proc/net/dev").ok()?;
    text.lines().skip(2).find_map(|line| {
        let (name, rest) = line.split_once(':')?;
        let name = name.trim();
        if name == "lo" {
            return None;
        }
        let fields: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|value| value.parse().ok())
            .collect();
        Some((name.to_owned(), *fields.first()?, *fields.get(8)?))
    })
}

/// 接口 `name` 的 IPv4 地址与掩码：先问 `ip`，没有再问 `ifconfig`。
fn ip_and_mask(name: &str) -> Option<(String, String)> {
    if let Ok(output) = Command::new("/bin/ip")
        .args(["-4", "-o", "addr", "show", name])
        .output()
    {
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if let Some(cidr) = text.split_whitespace().skip_while(|w| *w != "inet").nth(1) {
            let (ip, prefix) = cidr.split_once('/')?;
            let prefix: u32 = prefix.parse().ok()?;
            let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
            return Some((ip.to_owned(), mask_text(mask)));
        }
    }
    let output = Command::new("/bin/ifconfig").arg(name).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let after = |key: &str| {
        text.split(key)
            .nth(1)?
            .split_whitespace()
            .next()
            .map(|value| value.trim_start_matches("addr:").to_owned())
    };
    Some((
        after("inet addr:").or_else(|| after("inet "))?,
        after("Mask:").or_else(|| after("netmask "))?,
    ))
}

fn mask_text(mask: u32) -> String {
    let [a, b, c, d] = mask.to_be_bytes();
    format!("{a}.{b}.{c}.{d}")
}

/// `/proc/net/route` 里接口 `name` 的默认网关（小端十六进制）。
fn gateway(name: &str) -> Option<String> {
    let text = fs::read_to_string("/proc/net/route").ok()?;
    text.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.first() != Some(&name) || fields.get(1) != Some(&"00000000") {
            return None;
        }
        let value = u32::from_str_radix(fields.get(2)?, 16).ok()?;
        let [a, b, c, d] = value.to_le_bytes();
        Some(format!("{a}.{b}.{c}.{d}"))
    })
}

fn dns() -> Option<String> {
    let text = fs::read_to_string("/etc/resolv.conf").ok()?;
    let servers: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("nameserver"))
        .map(str::trim)
        .collect();
    (!servers.is_empty()).then(|| servers.join(", "))
}

impl Data {
    pub fn read() -> Self {
        let interface = counters().map(|(name, ..)| name);
        let address = interface.as_deref().and_then(|name| {
            let (ip, mask) = ip_and_mask(name)?;
            Some(Address {
                ip,
                mask,
                gateway: gateway(name).unwrap_or_default(),
                dns: dns().unwrap_or_default(),
            })
        });
        Self { interface, address }
    }
}

/// 带千分位的字节数。
fn bytes_text(value: u64) -> String {
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

pub struct Network {
    _controls: Vec<Widget>,
    received: Widget,
    sent: Widget,
}
impl Network {
    pub fn build(sheet: &PropertySheet, data: Data) -> liteos_ui::Result<Self> {
        let general = sheet.page("常规")?;
        let connection = group(&general, "连接")?;
        let mut controls = Vec::new();
        for (title, text) in [
            (
                "状态：",
                if data.address.is_some() {
                    "已连接"
                } else {
                    "已断开"
                },
            ),
            ("接口：", data.interface.as_deref().unwrap_or("无")),
        ] {
            let (a, b) = pair(&connection, title, text)?;
            controls.extend([a, b]);
        }
        let activity = group(&general, "活动")?;
        let (a, sent) = pair(&activity, "发送：", "正在读取…")?;
        controls.push(a);
        let (a, received) = pair(&activity, "接收：", "正在读取…")?;
        controls.push(a);
        let support = sheet.page("支持")?;
        let section = group(&support, "连接状态")?;
        let address = data.address.as_ref();
        for (title, text) in [
            ("地址类型：", "DHCP 分配"),
            ("IP 地址：", address.map_or("—", |a| &a.ip)),
            ("子网掩码：", address.map_or("—", |a| &a.mask)),
            ("默认网关：", address.map_or("—", |a| &a.gateway)),
            ("DNS 服务器：", address.map_or("—", |a| &a.dns)),
        ] {
            let (a, b) = pair(&section, title, text)?;
            controls.extend([a, b]);
        }
        controls.push(label(&support, "地址由 network-service 配置。")?);
        controls.extend([general, connection, activity, support, section]);
        Ok(Self {
            _controls: controls,
            received,
            sent,
        })
    }
}
impl Page for Network {
    fn activity(&self, background: Background, clock: Clock) -> Option<Activity> {
        let (received, sent) = (self.received.clone(), self.sent.clone());
        Some(Box::pin(async move {
            loop {
                let Ok(work) = background.run(counters) else {
                    break;
                };
                let (rx, tx) = work.await.map_or((0, 0), |(_, rx, tx)| (rx, tx));
                if received
                    .set_text(&format!("{} 字节", bytes_text(rx)))
                    .is_err()
                    || sent.set_text(&format!("{} 字节", bytes_text(tx))).is_err()
                {
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
