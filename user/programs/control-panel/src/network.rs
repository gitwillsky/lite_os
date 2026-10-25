//! "本地连接 状态"：连接状态、收发字节与 TCP/IP 地址；只读，地址由 DHCP（network-service）分配。
use crate::{Icons, Page};
use canvas::Rect;
use classic_theme::SystemColor as C;
use classic_ui::{Ink, Painter};
use std::{fs, process::Command};

/// 启动时读到的地址信息；DHCP 租约变化后重新打开本窗口才会刷新。
struct Address {
    ip: String,
    mask: String,
    gateway: String,
    dns: String,
}

pub struct Network {
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

impl Network {
    pub fn new() -> Self {
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

impl Page for Network {
    fn title(&self) -> &'static str {
        "本地连接 状态"
    }
    fn tabs(&self) -> &'static [&'static str] {
        &["常规", "支持"]
    }
    fn icon(&self) -> &'static str {
        if self.address.is_some() {
            "network-local"
        } else {
            "network-disconnected"
        }
    }
    fn size(&self) -> (i32, i32) {
        (360, 380)
    }
    fn paint(&self, p: &mut Painter<'_>, page: Rect, tab: usize, _now: i64, _icons: &Icons) {
        let ink = Ink::Color(C::ButtonText);
        let row = |p: &mut Painter<'_>, group: Rect, index: i32, label: &str, value: &str| {
            let y = group.y + 22 + index * (p.line_height() + 8);
            p.text(group.x + 14, y, label, ink, false);
            let width = (p.text_width(value, false) + p.scale() - 1) / p.scale();
            p.text(group.right() - 14 - width, y, value, ink, false);
        };
        if tab == 0 {
            let connected = self.address.is_some();
            let group = Rect::new(page.x + 12, page.y + 12, page.width - 24, 70);
            p.group_box(group, "连接");
            row(
                p,
                group,
                0,
                "状态：",
                if connected { "已连接" } else { "已断开" },
            );
            row(
                p,
                group,
                1,
                "接口：",
                self.interface.as_deref().unwrap_or("无"),
            );
            let (received, sent) = counters().map_or((0, 0), |(_, rx, tx)| (rx, tx));
            let group = Rect::new(page.x + 12, page.y + 96, page.width - 24, 88);
            p.group_box(group, "活动");
            row(p, group, 0, "发送：", &format!("{} 字节", bytes_text(sent)));
            row(
                p,
                group,
                1,
                "接收：",
                &format!("{} 字节", bytes_text(received)),
            );
        } else {
            let group = Rect::new(page.x + 12, page.y + 12, page.width - 24, 160);
            p.group_box(group, "连接状态");
            let none = String::from("—");
            let address = self.address.as_ref();
            row(p, group, 0, "地址类型：", "DHCP 分配");
            row(p, group, 1, "IP 地址：", address.map_or(&none, |a| &a.ip));
            row(
                p,
                group,
                2,
                "子网掩码：",
                address.map_or(&none, |a| &a.mask),
            );
            row(
                p,
                group,
                3,
                "默认网关：",
                address
                    .filter(|a| !a.gateway.is_empty())
                    .map_or(&none, |a| &a.gateway),
            );
            row(
                p,
                group,
                4,
                "DNS 服务器：",
                address
                    .filter(|a| !a.dns.is_empty())
                    .map_or(&none, |a| &a.dns),
            );
        }
    }
    fn click(&mut self, _page: Rect, _tab: usize, _point: (i32, i32)) -> bool {
        false
    }
}
