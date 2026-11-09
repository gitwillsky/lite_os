//! “高级 TCP/IP 设置”的 DNS 页（Windows 2000 netcfgx 2322、2304）。

use crate::{child, ip_address::IpAddress, label};
use liteos_ui::{
    Dialogs, Event, EventKind, EventStream, Layout, MessageButtons, MessageIcon, SheetStyle,
    ValidationError, Widget, WidgetKind as Kind, WidgetSpec, Window, WindowOptions, state,
};
use network_config::{normalize_dns_suffix, valid_search_list};
use std::{fmt::Display, net::Ipv4Addr};

pub(super) struct Settings {
    pub addresses: Vec<Ipv4Addr>,
    pub suffix: String,
    pub search_list: Vec<String>,
    pub append_parent_suffixes: bool,
}

struct OrderedList {
    list: Widget,
    add: Widget,
    edit: Widget,
    remove: Widget,
    up: Widget,
    down: Widget,
}

impl OrderedList {
    fn new(parent: &Widget, labels: [&str; 3]) -> liteos_ui::Result<Self> {
        let grow = Layout {
            grow: 1,
            ..Default::default()
        };
        let row = child(parent, Kind::Row, "", grow)?;
        let list = child(&row, Kind::List, "", grow)?;
        let arrows = child(&row, Kind::Column, "", Layout::default())?;
        let arrow = Layout {
            width: 28,
            height: 22,
            ..Default::default()
        };
        let up = child(&arrows, Kind::Button, "↑", arrow)?;
        let down = child(&arrows, Kind::Button, "↓", arrow)?;
        let buttons = child(parent, Kind::Row, "", Layout::default())?;
        let size = Layout {
            width: 65,
            height: 21,
            ..Default::default()
        };
        let add = child(&buttons, Kind::Button, labels[0], size)?;
        let edit = child(&buttons, Kind::Button, labels[1], size)?;
        let remove = child(&buttons, Kind::Button, labels[2], size)?;
        Ok(Self {
            list,
            add,
            edit,
            remove,
            up,
            down,
        })
    }

    fn selected(&self, count: usize) -> liteos_ui::Result<Option<usize>> {
        Ok(self
            .list
            .selection()?
            .and_then(|row| usize::try_from(row).ok()?.checked_sub(1))
            .filter(|&index| index < count))
    }

    fn buttons(&self, count: usize, enabled: bool, can_add: bool) -> liteos_ui::Result<()> {
        let selected = self.selected(count)?;
        for (control, active) in [
            (&self.list, true),
            (&self.add, can_add),
            (&self.edit, selected.is_some()),
            (&self.remove, selected.is_some()),
            (&self.up, selected.is_some_and(|index| index > 0)),
            (&self.down, selected.is_some_and(|index| index + 1 < count)),
        ] {
            control.set_state(if enabled && active {
                0
            } else {
                state::DISABLED
            })?;
        }
        Ok(())
    }

    fn show(&self, values: &[impl Display], selected: Option<usize>) -> liteos_ui::Result<()> {
        let texts: Vec<_> = values.iter().map(ToString::to_string).collect();
        let rows: Vec<_> = texts
            .iter()
            .enumerate()
            .map(|(index, text)| (index as u64 + 1, text.as_str()))
            .collect();
        self.list.set_rows(&rows)?;
        self.list.select(selected.map(|index| index as u64 + 1))
    }

    // Some(None) 新增一项，Some(Some(index)) 编辑现有项。
    fn editor(
        &self,
        event: &Event,
        count: usize,
        can_add: bool,
    ) -> liteos_ui::Result<Option<Option<usize>>> {
        if event.node == self.add.id() && can_add {
            return Ok(Some(None));
        }
        if event.node == self.edit.id() || event.node == self.list.id() {
            return Ok(self.selected(count)?.map(Some));
        }
        Ok(None)
    }

    fn update<T: Display>(
        &self,
        values: &mut Vec<T>,
        index: Option<usize>,
        value: T,
    ) -> liteos_ui::Result<()> {
        let index = if let Some(index) = index {
            values[index] = value;
            index
        } else {
            values.push(value);
            values.len() - 1
        };
        self.show(values, Some(index))
    }

    fn reorder(&self, event: &Event, values: &mut Vec<impl Display>) -> liteos_ui::Result<()> {
        let Some(index) = self.selected(values.len())? else {
            return Ok(());
        };
        let selected = if event.node == self.remove.id() {
            values.remove(index);
            values.len().checked_sub(1).map(|last| index.min(last))
        } else if event.node == self.up.id() && index > 0 {
            values.swap(index, index - 1);
            Some(index - 1)
        } else if event.node == self.down.id() && index + 1 < values.len() {
            values.swap(index, index + 1);
            Some(index + 1)
        } else {
            return Ok(());
        };
        self.show(values, selected)
    }
}

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    mut addresses: Vec<Ipv4Addr>,
    suffix: &str,
    search: &[String],
    mut append_parent_suffixes: bool,
) -> liteos_ui::Result<Option<Settings>> {
    let (sheet, mut events) =
        dialogs.property_sheet(parent, "高级 TCP/IP 设置", (386, 445), SheetStyle::NoApply)?;
    let page = sheet.page("DNS")?;
    let _servers_label = label(&page, "DNS 服务器地址(按使用顺序排列)(N):")?;
    let servers = OrderedList::new(&page, ["添加(A)...", "编辑(E)...", "删除(V)"])?;
    let automatic = child(
        &page,
        Kind::Radio,
        "附加主要的和连接特定的 DNS 后缀(P)",
        Layout::default(),
    )?;
    let parents = child(
        &page,
        Kind::CheckBox,
        "附加主 DNS 后缀的父后缀(X)",
        Layout::default(),
    )?;
    let ordered = child(
        &page,
        Kind::Radio,
        "附加这些 DNS 后缀(按顺序)(H):",
        Layout::default(),
    )?;
    let domains = OrderedList::new(&page, ["添加(D)...", "编辑(T)...", "删除(M)"])?;
    let _suffix_label = label(&page, "此连接的 DNS 后缀(S):")?;
    let suffix = child(&page, Kind::Edit, suffix, Layout::default())?;
    let mut search = search.to_vec();
    let mut manual = !search.is_empty();
    servers.show(&addresses, (!addresses.is_empty()).then_some(0))?;
    domains.show(&search, (!search.is_empty()).then_some(0))?;
    let mut validated = None;
    loop {
        automatic.set_state(if manual { 0 } else { state::CHECKED })?;
        ordered.set_state(if manual { state::CHECKED } else { 0 })?;
        parents.set_state(
            (if append_parent_suffixes {
                state::CHECKED
            } else {
                0
            }) | (if manual { state::DISABLED } else { 0 }),
        )?;
        servers.buttons(addresses.len(), true, true)?;
        domains.buttons(search.len(), manual, true)?;
        let Some(event) = events.next().await? else {
            break;
        };
        match event.kind {
            EventKind::Close => break,
            EventKind::Validate { token } => {
                let text = suffix.text()?;
                let result = normalize_dns_suffix(&text)
                    .map(str::to_owned)
                    .ok_or_else(|| ValidationError {
                        page: Some(page.clone()),
                        control: Some(suffix.clone()),
                        message: "DNS 后缀无效。请输入以句点分隔的 DNS 域名。".into(),
                    })
                    .and_then(|value| {
                        if manual && search.is_empty() {
                            return Err(ValidationError {
                                page: Some(page.clone()),
                                control: Some(domains.add.clone()),
                                message: "请添加至少一个 DNS 搜索后缀。".into(),
                            });
                        }
                        Ok(value)
                    });
                validated = result.as_ref().ok().cloned();
                sheet.validated(token, result.map(drop))?;
            }
            EventKind::Apply { token } => {
                sheet.applied(token, Ok(()))?;
                return Ok(validated.take().map(|suffix| Settings {
                    addresses,
                    suffix,
                    search_list: if manual { search } else { Vec::new() },
                    append_parent_suffixes,
                }));
            }
            EventKind::Activate { .. } if event.node == automatic.id() => manual = false,
            EventKind::Activate { .. } if event.node == ordered.id() => manual = true,
            EventKind::Activate { .. } if event.node == parents.id() && !manual => {
                append_parent_suffixes = parents.state()? & state::CHECKED != 0;
            }
            EventKind::Activate { .. } => {
                if let Some(index) = servers.editor(&event, addresses.len(), true)? {
                    if let Some(address) =
                        edit_address(dialogs, sheet.window(), &addresses, index).await?
                    {
                        servers.update(&mut addresses, index, address)?;
                    }
                } else if manual && let Some(index) = domains.editor(&event, search.len(), true)? {
                    if let Some(domain) =
                        edit_domain(dialogs, sheet.window(), &search, index).await?
                    {
                        domains.update(&mut search, index, domain)?;
                    }
                } else {
                    servers.reorder(&event, &mut addresses)?;
                    if manual {
                        domains.reorder(&event, &mut search)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

struct EntryDialog {
    window: Window,
    events: EventStream,
    row: Widget,
    _prompt: Widget,
    ok: Widget,
    cancel: Widget,
}

fn entry_dialog(
    dialogs: &Dialogs,
    parent: &Window,
    title: &str,
    label_text: &str,
) -> liteos_ui::Result<EntryDialog> {
    let mut options = WindowOptions::new(title, "org.liteos.control-panel.network", 320, 94);
    options.parent = Some(parent);
    options.modal = true;
    options.fixed = true;
    let (window, events) = dialogs.window(options)?;
    let mut spec = WidgetSpec::new(Kind::Column, "");
    spec.layout = Layout {
        grow: 1,
        padding: 10,
        gap: 9,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    let row = child(&root, Kind::Row, "", Layout::default())?;
    let prompt = child(&row, Kind::Label, label_text, Layout::default())?;
    let buttons = child(&root, Kind::Row, "", Layout::default())?;
    let _spacer = child(
        &buttons,
        Kind::Column,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    let size = Layout {
        width: 75,
        height: 22,
        ..Default::default()
    };
    let ok = child(&buttons, Kind::Button, "确定", size)?;
    ok.set_state(state::DEFAULT)?;
    let cancel = child(&buttons, Kind::Button, "取消", size)?;
    Ok(EntryDialog {
        window,
        events,
        row,
        _prompt: prompt,
        ok,
        cancel,
    })
}

async fn edit_domain(
    dialogs: &Dialogs,
    parent: &Window,
    search: &[String],
    selected: Option<usize>,
) -> liteos_ui::Result<Option<String>> {
    const TITLE: &str = "TCP/IP 域后缀";
    let mut dialog = entry_dialog(dialogs, parent, TITLE, "域后缀(D):")?;
    let field = child(
        &dialog.row,
        Kind::Edit,
        selected.map_or("", |index| &search[index]),
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    field.focus()?;
    while let Some(event) = dialog.events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == dialog.cancel.id() => break,
            EventKind::Activate { .. } if event.node == dialog.ok.id() => {
                let text = field.text()?;
                let result = normalize_dns_suffix(&text)
                    .filter(|name| !name.is_empty())
                    .ok_or("请输入有效的 DNS 域名。")
                    .and_then(|value| {
                        let mut list = search.to_vec();
                        if list.iter().enumerate().any(|(index, domain)| {
                            Some(index) != selected && domain.eq_ignore_ascii_case(value)
                        }) {
                            return Err("此 DNS 后缀已在列表中。");
                        }
                        if let Some(index) = selected {
                            list[index] = value.to_owned();
                        } else {
                            list.push(value.to_owned());
                        }
                        valid_search_list(&list)
                            .then_some(value)
                            .ok_or("DNS 搜索列表太长。")
                    });
                match result {
                    Ok(value) => return Ok(Some(value.to_owned())),
                    Err(message) => {
                        let _ = dialogs
                            .message(
                                &dialog.window,
                                TITLE,
                                message,
                                MessageButtons::Ok,
                                MessageIcon::Warning,
                            )?
                            .await?;
                        field.focus()?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

async fn edit_address(
    dialogs: &Dialogs,
    parent: &Window,
    addresses: &[Ipv4Addr],
    selected: Option<usize>,
) -> liteos_ui::Result<Option<Ipv4Addr>> {
    const TITLE: &str = "TCP/IP DNS 服务器";
    let mut dialog = entry_dialog(dialogs, parent, TITLE, "DNS 服务器(D):")?;
    let field = IpAddress::new(&dialog.row)?;
    field.set(selected.map(|index| addresses[index]))?;
    field.focus()?;
    while let Some(event) = dialog.events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == dialog.cancel.id() => break,
            EventKind::Activate { .. } if event.node == dialog.ok.id() => {
                let (message, control) =
                    match field.value() {
                        Err(control) => ("无效的 DNS 服务器地址。", Some(control)),
                        Ok(Some(address)) if !address.is_unspecified() => {
                            if !addresses.iter().enumerate().any(|(index, current)| {
                                Some(index) != selected && *current == address
                            }) {
                                return Ok(Some(address));
                            }
                            ("此 DNS 服务器地址已在列表中。", None)
                        }
                        Ok(_) => ("请输入有效的 DNS 服务器地址。", None),
                    };
                let _ = dialogs
                    .message(
                        &dialog.window,
                        TITLE,
                        message,
                        MessageButtons::Ok,
                        MessageIcon::Warning,
                    )?
                    .await?;
                if let Some(control) = control {
                    control.focus()?;
                } else {
                    field.focus()?;
                }
            }
            _ if field.owns(&event) => field.event(&event)?,
            _ => {}
        }
    }
    Ok(None)
}
