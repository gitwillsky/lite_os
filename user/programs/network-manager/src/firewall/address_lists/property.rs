//! 单个静态地址列表成员的编辑窗口。

use super::catalog::{Member, parse_address};
use crate::{
    child,
    item_list::{Glyph, Item, Tool, note, set_note, toolbar},
    item_properties::{Footer, Request, field},
    workspace::Workspace,
};
use liteos_ui::{Event, Layout, Widget, WidgetKind, state};
use network_config::{
    Revision,
    configuration::{
        ConfigurationName, connections::Family, valid_address_set_name, valid_firewall_prefix,
    },
};

pub(super) struct Property {
    pub(super) frame: Widget,
    pub(super) revision: Revision,
    pub(super) original: Option<Member>,
    name: Widget,
    address: Widget,
    actions: Widget,
    remove: Tool,
    message: Widget,
    footer: Footer,
    _layout: Vec<Widget>,
}

impl Property {
    pub(super) fn new(
        workspace: &Workspace,
        revision: Revision,
        entry: Option<Member>,
        suggested_name: Option<&ConfigurationName>,
    ) -> liteos_ui::Result<Self> {
        let title = entry.as_ref().map_or_else(
            || "新建防火墙地址列表".into(),
            |entry| format!("防火墙地址列表 > {}", entry.name.as_str()),
        );
        let frame = workspace.item_window(&title, (510, 230))?;
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
        let fields = child(&body, WidgetKind::Column, "", Layout { gap: 4, ..grow })?;
        let mut layout = Vec::new();
        let name = field(
            &fields,
            &mut layout,
            "列表",
            WidgetKind::Edit,
            entry
                .as_ref()
                .map(|entry| &entry.name)
                .or(suggested_name)
                .map_or("", ConfigurationName::as_str),
            false,
        )?;
        let address = field(
            &fields,
            &mut layout,
            "地址",
            WidgetKind::Edit,
            &entry
                .as_ref()
                .map_or_else(String::new, |entry| entry.address().to_string()),
            false,
        )?;
        let actions = child(
            &body,
            WidgetKind::Column,
            "",
            Layout {
                width: 94,
                ..Layout::default()
            },
        )?;
        let [remove] = toolbar(&actions, [Item::Command(Glyph::Remove, "删除")])?;
        let message = note(&root)?;
        let footer = Footer::new(&root)?;
        layout.extend([root, body, fields]);
        let property = Self {
            frame,
            revision,
            original: entry,
            name,
            address,
            actions,
            remove,
            message,
            footer,
            _layout: layout,
        };
        property.ready(true, true)?;
        if suggested_name.is_some() && property.original.is_none() {
            property.address.focus()?;
        } else {
            property.name.focus()?;
        }
        Ok(property)
    }

    pub(super) fn request(&self, event: &Event) -> Option<Request> {
        self.footer.request(&self.frame, event).or_else(|| {
            (self.remove.activated(event) && self.original.is_some()).then_some(Request::Remove)
        })
    }

    pub(super) fn value(&self, family: Family) -> liteos_ui::Result<Option<Member>> {
        let name = ConfigurationName::try_from(self.name.text()?)
            .ok()
            .filter(|name| valid_address_set_name(name.as_str()));
        let Some(name) = name else {
            self.note("请输入有效的列表名称。")?;
            self.name.focus()?;
            return Ok(None);
        };
        let address = self.address.text()?;
        let prefix = parse_address(&address)
            .filter(|prefix| family.includes(prefix.network) && valid_firewall_prefix(*prefix));
        let Some(prefix) = prefix else {
            self.note("请输入本地址族的 IP 地址或规范 CIDR 网段。")?;
            self.address.focus()?;
            return Ok(None);
        };
        Ok(Some(Member { name, prefix }))
    }

    pub(super) fn note(&self, text: &str) -> liteos_ui::Result<()> {
        set_note(&self.message, text)
    }

    pub(super) fn ready(&self, close: bool, save: bool) -> liteos_ui::Result<()> {
        self.actions.set_state(if self.original.is_some() {
            0
        } else {
            state::HIDDEN
        })?;
        let state = if save { 0 } else { state::DISABLED };
        self.name.set_state(state)?;
        self.address.set_state(state)?;
        self.remove.set_enabled(save)?;
        self.footer.set_enabled(close, save)
    }

    pub(super) fn bind(&mut self, revision: Revision, member: Member) -> liteos_ui::Result<()> {
        self.frame
            .set_text(&format!("防火墙地址列表 > {}", member.name.as_str()))?;
        self.revision = revision;
        self.original = Some(member);
        self.ready(true, true)
    }
}
