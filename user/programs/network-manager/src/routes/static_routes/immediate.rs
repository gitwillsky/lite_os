//! 路由属性中的立即网关引用。列表和详情读取同一份已解析成员。

use super::installed::{gateways, hop_address};
use crate::{
    child,
    item_list::spacer,
    item_properties::{button, field, reference_field},
    workspace::Workspace,
};
use liteos_ui::{
    Event, EventKind, Layout, Menu, MenuItem, Widget, WidgetKind, WidgetSpec, Window, state,
};
use network_config::routes::NextHop;
use std::net::IpAddr;

pub(super) struct ImmediateGateway {
    value: Widget,
    command: Widget,
    popup: Option<Widget>,
    detail: Option<Detail>,
}

impl ImmediateGateway {
    pub(super) fn new(
        parent: &Widget,
        layout: &mut Vec<Widget>,
        members: &[NextHop],
    ) -> liteos_ui::Result<Self> {
        let (value, command) = reference_field(parent, layout, "立即网关")?;
        let mut field = Self {
            value,
            command,
            popup: None,
            detail: None,
        };
        field.present(members)?;
        Ok(field)
    }

    pub(super) fn present(&mut self, members: &[NextHop]) -> liteos_ui::Result<()> {
        // 菜单项位置只解释当前成员序列。序列变化时撤销旧菜单，防止延迟事件打开另一条路径。
        self.popup = None;
        if self
            .detail
            .as_ref()
            .is_some_and(|detail| !members.contains(&detail.hop))
        {
            self.detail = None;
        }
        self.command
            .set_state(if members.is_empty() { state::HIDDEN } else { 0 })?;
        self.value.set_text(&gateways(members, true))
    }

    pub(super) fn handle(
        &mut self,
        workspace: &Workspace,
        window: &Window,
        event: &Event,
        members: &[NextHop],
    ) -> liteos_ui::Result<bool> {
        if self
            .detail
            .as_ref()
            .is_some_and(|detail| detail.closes(event))
        {
            self.detail = None;
            return Ok(true);
        }
        if self
            .popup
            .as_ref()
            .is_some_and(|popup| popup.id() == event.node)
        {
            match event.kind {
                EventKind::Activate { value } => {
                    self.popup = None;
                    let Some(hop) = value
                        .checked_sub(2)
                        .and_then(|index| usize::try_from(index).ok())
                        .and_then(|index| members.get(index).copied())
                    else {
                        return Ok(true);
                    };
                    if let Some(detail) = &self.detail
                        && detail.hop == hop
                    {
                        workspace.raise(&detail.frame)?;
                    } else {
                        self.detail = Some(Detail::new(workspace, hop)?);
                    }
                }
                EventKind::Dismiss => self.popup = None,
                _ => {}
            }
            return Ok(true);
        }
        if self.command.id() == event.node
            && matches!(event.kind, EventKind::Activate { .. })
            && !members.is_empty()
        {
            self.open_menu(window, members)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn open_menu(&mut self, window: &Window, members: &[NextHop]) -> liteos_ui::Result<()> {
        let commands: Vec<_> = members.iter().map(|hop| hop_address(hop, true)).collect();
        let items: Vec<_> = std::iter::once(MenuItem {
            id: 1,
            label: "下一跳",
            shortcut: "",
            state: 1,
            parent: 0,
        })
        .chain(commands.iter().enumerate().map(|(index, label)| MenuItem {
            id: index as u64 + 2,
            label,
            shortcut: "",
            state: 0,
            parent: 0,
        }))
        .collect();
        let popup = window.widget(WidgetSpec::new(WidgetKind::PopupMenu, ""))?;
        popup.set_menus(&[Menu {
            title: "",
            items: &items,
        }])?;
        let bounds = self.command.geometry()?;
        popup.open_popup(Some((bounds.x, bounds.bottom())))?;
        self.popup = Some(popup);
        Ok(())
    }
}

struct Detail {
    frame: Widget,
    close: Widget,
    hop: NextHop,
    _layout: Vec<Widget>,
}

impl Detail {
    fn new(workspace: &Workspace, hop: NextHop) -> liteos_ui::Result<Self> {
        let address = hop_address(&hop, true);
        let frame = workspace.item_window(&format!("下一跳 > {address}"), (395, 178))?;
        let root = child(
            &frame,
            WidgetKind::Column,
            "",
            Layout {
                grow: 1,
                gap: 4,
                padding: 6,
                ..Layout::default()
            },
        )?;
        let afi = match hop.gateway {
            Some(IpAddr::V4(_)) => "ip",
            Some(IpAddr::V6(_)) => "ip6",
            None => "link",
        };
        let mut layout = Vec::new();
        let value = field(&root, &mut layout, "AFI", WidgetKind::Edit, afi, true)?;
        layout.push(value);
        let gateway = child(
            &root,
            WidgetKind::Group,
            "立即网关",
            Layout {
                grow: 1,
                padding: 5,
                gap: 4,
                ..Layout::default()
            },
        )?;
        for (label, value) in [("网关地址", address), ("权重", hop.weight.to_string())] {
            let input = field(&gateway, &mut layout, label, WidgetKind::Edit, &value, true)?;
            layout.push(input);
        }
        let footer = child(&root, WidgetKind::Row, "", Layout::default())?;
        let space = spacer(&footer)?;
        let close = child(&footer, WidgetKind::Button, "取消", button())?;
        close.set_state(state::DEFAULT)?;
        layout.extend([root, gateway, footer, space]);
        Ok(Self {
            frame,
            close,
            hop,
            _layout: layout,
        })
    }

    fn closes(&self, event: &Event) -> bool {
        matches!(event.kind, EventKind::Activate { .. })
            && [self.frame.id(), self.close.id()].contains(&event.node)
    }
}
