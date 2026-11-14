//! 选中接口的实时流量构成与近期变化。

use super::volume;
use crate::{APP_ID, child, observation::receive};
use liteos_ui::{
    Application, Background, Clock, Column, EventKind, Layout, ListOptions, ListStyle, Task,
    Widget, WidgetKind, WidgetSpec, Window, WindowOptions, state,
};
use network_config::traffic::{Endpoints, Flow, FlowObservation, Sample};
use std::{
    collections::VecDeque,
    future::{Future, pending, poll_fn},
    net::SocketAddr,
    os::fd::AsFd,
    pin::pin,
};

const HISTORY: usize = 10;

pub(super) fn open(
    app: &mut Application,
    parent: &Window,
    interface: u32,
    name: &str,
    background: Background,
) -> liteos_ui::Result<Task<()>> {
    let title = format!("流量分析 - {name}");
    let mut options = WindowOptions::new(&title, APP_ID, 820, 450);
    options.parent = Some(parent);
    options.modal = true;
    let (window, mut events) = app.dialogs().window(options)?;
    let root = window.widget(WidgetSpec {
        layout: Layout {
            grow: 1,
            gap: 6,
            padding: 8,
            ..Layout::default()
        },
        ..WidgetSpec::new(WidgetKind::Column, "")
    })?;
    let status = child(&root, WidgetKind::Label, "正在采集流量…", Layout::default())?;
    let flows = table(
        &root,
        &[
            ("方向", 55),
            ("协议", 80),
            ("源", 188),
            ("目的", 188),
            ("速率", 126),
            ("占比", 105),
        ],
    )?;
    let _history_label = child(
        &root,
        WidgetKind::Label,
        "最近 10 次采样",
        Layout::default(),
    )?;
    let history = table(
        &root,
        &[
            ("时间", 106),
            ("接收速率", 145),
            ("发送速率", 145),
            ("采集丢包", 110),
        ],
    )?;
    let close = child(
        &root,
        WidgetKind::Button,
        "关闭",
        Layout {
            width: 86,
            height: 23,
            ..Layout::default()
        },
    )?;
    close.set_state(state::DEFAULT)?;
    let view = View {
        status,
        flows,
        history,
        clock: app.clock(),
    };
    app.spawn_local(parent, async move {
        let mut closing = pin!(async {
            while let Ok(Some(event)) = events.next().await {
                if matches!(
                    event.kind,
                    EventKind::Close
                        | EventKind::Key {
                            code: 1,
                            pressed: true,
                            ..
                        }
                ) || event.node == close.id() && matches!(event.kind, EventKind::Activate { .. })
                {
                    break;
                }
            }
        });
        let mut observing = pin!(async {
            if let Err(error) = view.observe(&window, interface, background).await {
                let _ = view.status.set_text(&format!("流量观察已停止。{error}"));
            }
            pending::<()>().await;
        });
        poll_fn(|cx| {
            if closing.as_mut().poll(cx).is_ready() {
                return std::task::Poll::Ready(());
            }
            observing.as_mut().poll(cx)
        })
        .await;
        let _ = window.close();
    })
}

fn table(parent: &Widget, columns: &[(&str, i32)]) -> liteos_ui::Result<Widget> {
    let list = child(
        parent,
        WidgetKind::List,
        "",
        Layout {
            grow: 1,
            ..Layout::default()
        },
    )?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: false,
            full_row_select: true,
        },
    )?;
    let columns: Vec<_> = columns
        .iter()
        .enumerate()
        .map(|(index, &(title, width))| Column {
            id: index as u64 + 1,
            title,
            width,
        })
        .collect();
    list.set_columns(&columns)?;
    Ok(list)
}

struct View {
    status: Widget,
    flows: Widget,
    history: Widget,
    clock: Clock,
}

impl View {
    async fn observe(
        &self,
        window: &Window,
        interface: u32,
        background: Background,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut stream = background
            .run(move || FlowObservation::start(interface))?
            .await?;
        let mut fd = window.watch(stream.as_fd().try_clone_to_owned()?)?;
        let mut history = VecDeque::with_capacity(HISTORY);
        let mut elapsed = 0u64;
        loop {
            let sample = receive(&self.clock, &mut fd, stream.remaining(), || {
                stream.try_next()
            })
            .await?;
            elapsed = elapsed.saturating_add(sample.interval_ns);
            let [rx, tx] = sample.bytes.map(|bytes| rate(bytes, sample.interval_ns));
            if history.len() == HISTORY {
                history.pop_back();
            }
            history.push_front(format!(
                "{:.1} 秒\t{rx}\t{tx}\t{}",
                elapsed as f64 / 1e9,
                sample.drops
            ));
            self.present(&sample)?;
            let rows: Vec<_> = history
                .iter()
                .enumerate()
                .map(|(index, text)| (index as u64 + 1, text.as_str()))
                .collect();
            self.history.set_rows(&rows)?;
        }
    }

    fn present(&self, sample: &Sample) -> liteos_ui::Result<()> {
        let total: u64 = sample.bytes.iter().sum();
        let attributed: u64 = sample.rows.iter().map(|row| row.minimum_bytes).sum();
        let [rx, tx] = sample.bytes.map(|bytes| rate(bytes, sample.interval_ns));
        self.status.set_text(&format!(
            "接收 {rx}　发送 {tx}　其他或未确定 {}　采集丢包 {}",
            rate(total - attributed, sample.interval_ns),
            sample.drops
        ))?;
        let text: Vec<_> = sample
            .rows
            .iter()
            .map(|row| flow_text(row, sample.interval_ns, total))
            .collect();
        let rows: Vec<_> = text
            .iter()
            .enumerate()
            .map(|(index, text)| (index as u64 + 1, text.as_str()))
            .collect();
        self.flows.set_rows(&rows)
    }
}

fn flow_text(flow: &Flow, interval: u64, total: u64) -> String {
    let direction = if flow.key.outgoing {
        "发送"
    } else {
        "接收"
    };
    let (protocol, source, destination) = match flow.key.endpoints {
        Endpoints::Ip {
            source,
            destination,
            protocol,
            ports,
        } => {
            let protocol = match protocol {
                1 => "ICMP".into(),
                6 => "TCP".into(),
                17 => "UDP".into(),
                58 => "ICMPv6".into(),
                132 => "SCTP".into(),
                protocol => format!("IP {protocol}"),
            };
            match ports {
                Some([from, to]) => (
                    protocol,
                    SocketAddr::new(source, from).to_string(),
                    SocketAddr::new(destination, to).to_string(),
                ),
                None => (protocol, source.to_string(), destination.to_string()),
            }
        }
        Endpoints::Ethernet {
            source,
            destination,
            ether_type,
        } => {
            let protocol = match ether_type {
                0x0806 => "ARP".into(),
                value => format!("0x{value:04x}"),
            };
            (protocol, mac(source), mac(destination))
        }
    };
    let [minimum, maximum] = [flow.minimum_bytes, flow.maximum_bytes];
    let rate = rate(minimum, interval);
    let share = minimum as f64 * 100.0 / total as f64;
    let prefix = if minimum != maximum { "≥ " } else { "" };
    format!("{direction}\t{protocol}\t{source}\t{destination}\t{prefix}{rate}\t{prefix}{share:.1}%")
}

fn mac(bytes: [u8; 6]) -> String {
    let [a, b, c, d, e, f] = bytes;
    format!("{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}")
}

fn rate(bytes: u64, interval: u64) -> String {
    let rate = u128::from(bytes) * 1_000_000_000 / u128::from(interval);
    volume(rate.min(u128::from(u64::MAX)) as u64, "/s")
}
