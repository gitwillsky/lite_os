//! 转发连接的只读常规属性。字段与表格使用同一份连接记录。

use crate::{
    child,
    item_list::{Glyph, Item, Tool, spacer, toolbar},
    item_properties::{button, field},
    workspace::Workspace,
};
use liteos_ui::{Event, Layout, Widget, WidgetKind, state};
use network_config::configuration::connections::{Connection, Identity, Phase, Protocol};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
    Flags,
    Source,
    SourcePort,
    Destination,
    DestinationPort,
    ReplySource,
    ReplySourcePort,
    ReplyDestination,
    ReplyDestinationPort,
    Protocol,
    TcpState,
}

impl Field {
    pub(super) const COLUMNS: [(Self, i32); 9] = [
        (Self::Flags, 42),
        (Self::Source, 146),
        (Self::SourcePort, 66),
        (Self::Destination, 146),
        (Self::DestinationPort, 66),
        (Self::ReplySource, 146),
        (Self::ReplyDestination, 146),
        (Self::Protocol, 76),
        (Self::TcpState, 83),
    ];

    const GENERAL: [Self; 10] = [
        Self::Source,
        Self::SourcePort,
        Self::Destination,
        Self::DestinationPort,
        Self::ReplySource,
        Self::ReplySourcePort,
        Self::ReplyDestination,
        Self::ReplyDestinationPort,
        Self::Protocol,
        Self::TcpState,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Flags => "标志",
            Self::Source => "源地址",
            Self::SourcePort => "源端口",
            Self::Destination => "目的地址",
            Self::DestinationPort => "目的端口",
            Self::ReplySource => "回复源地址",
            Self::ReplySourcePort => "回复源端口",
            Self::ReplyDestination => "回复目的地址",
            Self::ReplyDestinationPort => "回复目的端口",
            Self::Protocol => "协议",
            Self::TcpState => "TCP 状态",
        }
    }

    pub(super) fn text(self, row: &Connection) -> String {
        let port = |port: u16| {
            if row.protocol == Protocol::Echo {
                String::new()
            } else {
                port.to_string()
            }
        };
        match self {
            Self::Flags => {
                let mut flags = String::with_capacity(4);
                if row.seen_reply {
                    flags.push('S');
                }
                // 所有记录都在首包成功提交后公布。
                flags.push('C');
                if row.source != row.reply_destination {
                    flags.push('s');
                }
                if row.destination != row.reply_source {
                    flags.push('d');
                }
                flags
            }
            Self::Source => row.source.ip().to_string(),
            Self::SourcePort => port(row.source.port()),
            Self::Destination => row.destination.ip().to_string(),
            Self::DestinationPort => port(row.destination.port()),
            Self::ReplySource => row.reply_source.ip().to_string(),
            Self::ReplySourcePort => port(row.reply_source.port()),
            Self::ReplyDestination => row.reply_destination.ip().to_string(),
            Self::ReplyDestinationPort => port(row.reply_destination.port()),
            Self::Protocol => {
                let (number, name) = protocol_number_and_name(row);
                format!("{number} ({name})")
            }
            Self::TcpState if row.protocol != Protocol::Tcp => String::new(),
            Self::TcpState => match row.phase {
                Phase::Opening => "握手中",
                Phase::Established => "established",
                Phase::Closing => "关闭中",
                Phase::Reset => "close",
            }
            .into(),
        }
    }
}

pub(super) fn protocol_number_and_name(row: &Connection) -> (u8, &'static str) {
    match row.protocol {
        Protocol::Tcp => (6, "tcp"),
        Protocol::Udp => (17, "udp"),
        Protocol::Echo if row.source.is_ipv4() => (1, "icmp"),
        Protocol::Echo => (58, "ipv6-icmp"),
    }
}

pub(super) struct Property {
    pub(super) frame: Widget,
    pub(super) identity: Identity,
    pub(super) remove: Tool,
    values: Vec<(Field, Widget)>,
    seen_reply: Widget,
    source_nat: Widget,
    destination_nat: Widget,
    cancel: Widget,
    _layout: Vec<Widget>,
}

impl Property {
    pub(super) fn new(
        workspace: &Workspace,
        identity: Identity,
        row: &Connection,
    ) -> liteos_ui::Result<Self> {
        let title = format!("连接 > {} → {}", row.source.ip(), row.destination.ip());
        let frame = workspace.item_window(&title, (495, 376))?;
        let grow = Layout {
            grow: 1,
            ..Layout::default()
        };
        let root = child(
            &frame,
            WidgetKind::Column,
            "",
            Layout {
                padding: 4,
                gap: 4,
                ..grow
            },
        )?;
        let body = child(&root, WidgetKind::Row, "", Layout { gap: 6, ..grow })?;
        let tabs = child(&body, WidgetKind::Tabs, "", grow)?;
        let general = child(
            &tabs,
            WidgetKind::Column,
            "常规",
            Layout {
                padding: 7,
                gap: 3,
                ..grow
            },
        )?;
        let actions = child(
            &body,
            WidgetKind::Column,
            "",
            Layout {
                width: 78,
                ..Layout::default()
            },
        )?;
        let [remove] = toolbar(&actions, [Item::Command(Glyph::Remove, "删除")])?;
        let mut layout = Vec::new();
        let mut values = Vec::new();
        for property in Field::GENERAL {
            if row.protocol == Protocol::Echo
                && matches!(
                    property,
                    Field::SourcePort
                        | Field::DestinationPort
                        | Field::ReplySourcePort
                        | Field::ReplyDestinationPort
                )
                || row.protocol != Protocol::Tcp && matches!(property, Field::TcpState)
            {
                continue;
            }
            let control = field(
                &general,
                &mut layout,
                property.label(),
                WidgetKind::Edit,
                "",
                true,
            )?;
            values.push((property, control));
        }
        let footer = child(
            &root,
            WidgetKind::Row,
            "",
            Layout {
                gap: 4,
                ..Layout::default()
            },
        )?;
        let badge = |label| {
            child(
                &footer,
                WidgetKind::Label,
                label,
                Layout {
                    padding: 2,
                    ..Layout::default()
                },
            )
        };
        let seen_reply = badge("已收到回复")?;
        let confirmed = badge("已确认")?;
        let source_nat = badge("源 NAT")?;
        let destination_nat = badge("目的 NAT")?;
        let space = spacer(&footer)?;
        let cancel = child(&footer, WidgetKind::Button, "取消", button())?;
        layout.extend([root, body, tabs, general, actions, footer, confirmed, space]);
        let property = Self {
            frame,
            identity,
            remove,
            values,
            seen_reply,
            source_nat,
            destination_nat,
            cancel,
            _layout: layout,
        };
        property.present(row)?;
        Ok(property)
    }

    pub(super) fn present(&self, row: &Connection) -> liteos_ui::Result<()> {
        for (field, control) in &self.values {
            control.set_text(&field.text(row))?;
        }
        for (badge, shown) in [
            (&self.seen_reply, row.seen_reply),
            (&self.source_nat, row.source != row.reply_destination),
            (&self.destination_nat, row.destination != row.reply_source),
        ] {
            badge.set_state(if shown { 0 } else { state::HIDDEN })?;
        }
        Ok(())
    }

    pub(super) fn closes(&self, event: &Event) -> bool {
        event.node == self.frame.id() || event.node == self.cancel.id()
    }
}
