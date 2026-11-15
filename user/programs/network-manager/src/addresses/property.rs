//! 单条双栈地址属性。运行地址只读，静态条目保存到同一路由域的接口配置。

use super::{
    Family,
    catalog::{Address, Catalog, Entry, Member, network},
    title,
};
use crate::{
    child,
    configuration::DraftBaseline,
    item_list::{Glyph, Item, Tool, note, set_note, toolbar},
    item_properties::{Footer, Request, field},
    workspace::Workspace,
};
use liteos_ui::{Event, EventKind, Layout, TreeRow, Widget, WidgetKind, state};
use network_config::configuration::Ipv6Address;
use std::net::Ipv4Addr;

pub(super) struct Property {
    pub(super) frame: Widget,
    pub(super) baseline: Option<DraftBaseline>,
    pub(super) original: Option<Member>,
    family: Family,
    address: Widget,
    network: Option<Widget>,
    interface: Widget,
    targets: Vec<super::catalog::Interface>,
    remove: Tool,
    actions: Widget,
    footer: Footer,
    message: Widget,
    read_only: bool,
    _decor: Vec<Widget>,
}

impl Property {
    pub(super) fn new(
        workspace: &Workspace,
        catalog: &Catalog,
        family: Family,
        entry: Option<&Entry>,
    ) -> liteos_ui::Result<Self> {
        let label = entry.map_or_else(
            || format!("新建{}", title(family)),
            |entry| format!("{} > {}", title(family), entry.address_text()),
        );
        let read_only =
            catalog.baseline.is_none() || entry.is_some_and(|entry| entry.member.is_none());
        let original = entry.and_then(|entry| entry.member.clone());
        let frame = workspace.item_window(&label, (510, 245))?;
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
        let mut decor = Vec::new();
        let address = field(
            &fields,
            &mut decor,
            "地址",
            WidgetKind::Edit,
            &entry.map_or_else(String::new, Entry::address_text),
            read_only,
        )?;
        let network = if family == Family::Ipv4 {
            Some(field(
                &fields,
                &mut decor,
                "网络",
                WidgetKind::Edit,
                &entry
                    .and_then(Entry::network)
                    .map_or_else(String::new, |network| network.to_string()),
                true,
            )?)
        } else {
            None
        };
        let interface = field(
            &fields,
            &mut decor,
            "接口",
            if read_only {
                WidgetKind::Edit
            } else {
                WidgetKind::Combo
            },
            entry.map_or("", |entry| entry.interface.as_str()),
            read_only,
        )?;
        let mut targets = Vec::new();
        if read_only {
            if family == Family::Ipv6
                && let Some((preferred, valid)) = entry.and_then(|entry| entry.runtime?.lifetimes)
            {
                for (label, duration) in [("有效寿命", valid), ("首选寿命", preferred)] {
                    let value = field(
                        &fields,
                        &mut decor,
                        label,
                        WidgetKind::Edit,
                        &lifetime(duration),
                        true,
                    )?;
                    decor.push(value);
                }
            }
        } else {
            interface.set_state(state::DROPDOWN_LIST)?;
            let choices: Vec<_> = catalog
                .targets
                .iter()
                .filter(|target| {
                    target.available
                        || original
                            .as_ref()
                            .is_some_and(|member| member.interface == target.interface)
                })
                .collect();
            let options: Vec<_> = choices
                .iter()
                .enumerate()
                .map(|(index, target)| TreeRow {
                    id: index as u64 + 1,
                    parent: 0,
                    text: &target.name,
                    expanded: false,
                    has_children: false,
                })
                .collect();
            interface.set_tree_rows(&options)?;
            let selected = original
                .as_ref()
                .and_then(|member| {
                    choices
                        .iter()
                        .position(|target| target.interface == member.interface)
                })
                .or_else(|| (options.len() == 1).then_some(0));
            interface.select(selected.map(|index| index as u64 + 1))?;
            targets.extend(choices.iter().map(|target| target.interface.clone()));
        }
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
        decor.extend([root, body, fields]);
        let property = Self {
            frame,
            baseline: catalog.baseline.clone(),
            original,
            family,
            address,
            network,
            interface,
            targets,
            remove,
            actions,
            footer,
            message,
            read_only,
            _decor: decor,
        };
        property.ready(true, true)?;
        if !read_only {
            property.address.focus()?;
        }
        Ok(property)
    }

    pub(super) fn request(&self, event: &Event) -> Option<Request> {
        if let Some(request) = self.footer.request(&self.frame, event) {
            return match request {
                Request::Save { close: true } if self.read_only => Some(Request::Close),
                Request::Save { .. } if self.read_only => None,
                request => Some(request),
            };
        }
        (self.remove.activated(event) && self.original.is_some()).then_some(Request::Remove)
    }

    pub(super) fn address_changed(&self, event: &Event) -> liteos_ui::Result<()> {
        if event.node == self.address.id()
            && matches!(event.kind, EventKind::Changed { .. })
            && let Some(network_field) = &self.network
            && !self.read_only
        {
            let value = match self.parse(&self.address.text()?) {
                Some(Address::Ipv4(address, prefix)) => network(address, prefix).to_string(),
                _ => String::new(),
            };
            network_field.set_text(&value)?;
        }
        Ok(())
    }

    fn parse(&self, text: &str) -> Option<Address> {
        let (address, prefix) = text.trim().split_once('/')?;
        let prefix: u8 = prefix.parse().ok()?;
        match self.family {
            Family::Ipv4 => {
                let address: Ipv4Addr = address.parse().ok()?;
                (prefix <= 32
                    && !address.is_unspecified()
                    && !address.is_loopback()
                    && !address.is_multicast()
                    && !address.is_broadcast())
                .then_some(Address::Ipv4(address, prefix))
            }
            Family::Ipv6 => {
                let value = Ipv6Address {
                    address: address.parse().ok()?,
                    prefix_length: prefix,
                    on_link: match self.original.as_ref().map(|member| member.address) {
                        Some(Address::Ipv6(address)) => address.on_link,
                        _ => true,
                    },
                };
                value.valid().then_some(Address::Ipv6(value))
            }
        }
    }

    pub(super) fn value(&self) -> liteos_ui::Result<Option<Member>> {
        let Some(address) = self.parse(&self.address.text()?) else {
            self.note(match self.family {
                Family::Ipv4 => "请输入 IPv4 单播地址和前缀长度，例如 192.0.2.1/24。",
                Family::Ipv6 => "请输入 IPv6 单播地址和前缀长度，例如 2001:db8::1/64。",
            })?;
            self.address.focus()?;
            return Ok(None);
        };
        let selected = self.interface.selection()?.and_then(|id| id.checked_sub(1));
        let Some(interface) = selected.and_then(|id| self.targets.get(id as usize)) else {
            self.note("请选择接口。")?;
            self.interface.focus()?;
            return Ok(None);
        };
        Ok(Some(Member {
            interface: interface.clone(),
            address,
        }))
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
        self.remove.set_enabled(save)?;
        if self.read_only {
            return self.footer.set_enabled(true, false);
        }
        let lock = if save { 0 } else { state::DISABLED };
        self.address.set_state(lock)?;
        self.interface.set_state(state::DROPDOWN_LIST | lock)?;
        self.footer.set_enabled(close, save)
    }

    pub(super) fn bind(
        &mut self,
        baseline: DraftBaseline,
        member: Member,
    ) -> liteos_ui::Result<()> {
        let (address, prefix) = member.address.parts();
        self.frame
            .set_text(&format!("{} > {address}/{prefix}", title(self.family)))?;
        self.baseline = Some(baseline);
        self.original = Some(member);
        self.ready(true, true)
    }
}

pub(super) fn lifetime(seconds: u32) -> String {
    if seconds == u32::MAX {
        return String::new();
    }
    format!("{seconds}s")
}
