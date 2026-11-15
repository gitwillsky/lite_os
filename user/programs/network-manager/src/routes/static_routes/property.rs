//! 双栈静态路由公用的工作区属性表单。配置中没有对应语义的 WinBox 字段不显示。

use super::{
    View,
    catalog::{Key, Selections, table_name, target_text},
    immediate::ImmediateGateway,
    installed::Installed,
};
use crate::{
    child,
    item_list::{note, set_note, spacer},
    item_properties::{button, field},
    workspace::Workspace,
};
use liteos_ui::{
    Background, Clock, Event, EventKind, Layout, TreeRow, Widget, WidgetKind, Window, state,
};
use network_config::{
    ManagementClient, Revision,
    configuration::{
        ConfigurationName, IpPrefix, StaticAction, StaticMember, StaticRoute, StaticTarget,
    },
    routes::{
        Family, NextHop,
        selection::{State, paths},
    },
};
use std::{cell::RefCell, net::IpAddr, rc::Rc, time::Duration};

pub(super) struct Binding {
    pub revision: Revision,
    pub original: Option<(Key, StaticRoute)>,
    pub table: u32,
}

pub(super) enum Request {
    Close,
    Save { close: bool },
    Remove,
}

pub(super) struct ObservedProperty {
    pub frame: Widget,
    pub entry: Installed,
    close: Widget,
    immediate: ImmediateGateway,
    _layout: Vec<Widget>,
}

impl ObservedProperty {
    pub(super) fn new(workspace: &Workspace, entry: &Installed) -> liteos_ui::Result<Self> {
        let prefix = entry.prefix();
        let gateway = entry.gateway();
        let frame = workspace.item_window(&format!("路由 > {prefix} → {gateway}"), (490, 314))?;
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
        let tabs = child(&root, WidgetKind::Tabs, "", grow)?;
        let general = child(
            &tabs,
            WidgetKind::Column,
            "常规",
            Layout {
                padding: 10,
                gap: 5,
                ..Layout::default()
            },
        )?;
        let mut layout = Vec::new();
        for (label, value) in [("目的地址", prefix), ("网关", gateway)] {
            let input = field(&general, &mut layout, label, WidgetKind::Edit, &value, true)?;
            layout.push(input);
        }
        let immediate = ImmediateGateway::new(&general, &mut layout, entry.members())?;
        for (label, value) in [
            ("距离", entry.distance().into()),
            ("路由表", table_name(entry.route.table)),
        ] {
            let input = field(&general, &mut layout, label, WidgetKind::Edit, &value, true)?;
            layout.push(input);
        }
        let ipv4 = entry.route.prefix.network.is_ipv4();
        let origin = entry.origin();
        if ipv4 {
            let status = StatusPage::new(&tabs, &mut layout, origin)?;
            status.present("活动", Some(entry.route.metric))?;
            layout.extend([status.contribution, status.cost]);
        }
        let footer = child(
            &root,
            WidgetKind::Row,
            "",
            Layout {
                gap: 6,
                ..Layout::default()
            },
        )?;
        if ipv4 {
            let flags = if matches!(entry.route.protocol, 3 | 4) {
                format!("活动  {origin}")
            } else {
                format!("动态  活动  {origin}")
            };
            layout.push(child(
                &footer,
                WidgetKind::Label,
                flags.trim_end(),
                Layout::default(),
            )?);
        }
        layout.push(spacer(&footer)?);
        let close = child(
            &footer,
            WidgetKind::Button,
            if ipv4 { "取消" } else { "确定" },
            button(),
        )?;
        close.set_state(state::DEFAULT)?;
        layout.extend([root, tabs, general, footer]);
        Ok(Self {
            frame,
            entry: entry.clone(),
            close,
            immediate,
            _layout: layout,
        })
    }

    pub(super) fn closes(&self, event: &Event) -> bool {
        matches!(event.kind, EventKind::Activate { .. })
            && [self.frame.id(), self.close.id()].contains(&event.node)
    }

    pub(super) fn handle_gateway(
        &mut self,
        workspace: &Workspace,
        window: &Window,
        event: &Event,
    ) -> liteos_ui::Result<bool> {
        self.immediate
            .handle(workspace, window, event, self.entry.members())
    }
}

struct StatusPage {
    contribution: Widget,
    cost: Widget,
}

impl StatusPage {
    fn new(tabs: &Widget, layout: &mut Vec<Widget>, origin: &str) -> liteos_ui::Result<Self> {
        let page = child(
            tabs,
            WidgetKind::Column,
            "状态",
            Layout {
                padding: 10,
                gap: 5,
                ..Layout::default()
            },
        )?;
        for (label, value) in [("所属", origin), ("AFI", "ip")] {
            let field = field(&page, layout, label, WidgetKind::Edit, value, true)?;
            layout.push(field);
        }
        let contribution = field(&page, layout, "参选状态", WidgetKind::Edit, "", true)?;
        let cost = field(&page, layout, "路由代价", WidgetKind::Edit, "", true)?;
        layout.push(page);
        Ok(Self { contribution, cost })
    }

    fn present(&self, contribution: &str, cost: Option<u32>) -> liteos_ui::Result<()> {
        let cost = cost.map(|cost| cost.to_string()).unwrap_or_default();
        for (field, value) in [
            (&self.contribution, contribution),
            (&self.cost, cost.as_str()),
        ] {
            if field.text()? != value {
                field.set_text(value)?;
            }
        }
        Ok(())
    }
}

pub(super) struct Editor {
    pub frame: Widget,
    // 标签、容器与空白只需存活。
    _layout: Vec<Widget>,
    enabled: Widget,
    destination: Widget,
    gateway: Widget,
    immediate: ImmediateGateway,
    members: Vec<NextHop>,
    distance: Widget,
    table: Widget,
    blackhole: Widget,
    cancel: Widget,
    apply: Widget,
    ok: Widget,
    panel: Widget,
    remove: Widget,
    message: Widget,
    status: Option<(StatusPage, Widget)>,
    pub binding: Binding,
}

impl Editor {
    pub(super) fn new(
        workspace: &Workspace,
        family: Family,
        tables: &[u32],
        binding: Binding,
        selection: Option<State>,
    ) -> liteos_ui::Result<Self> {
        let current = binding.original.as_ref().map(|(_, route)| route);
        let prefix = current.map_or_else(
            || match family {
                Family::Ipv4 => "0.0.0.0/0".to_owned(),
                Family::Ipv6 => "::/0".to_owned(),
            },
            |route| format!("{}/{}", route.destination, route.prefix_length),
        );
        let title = if let Some(route) = current {
            format!(
                "路由 > {prefix} → {}",
                super::catalog::gateway_text(&route.action)
            )
        } else {
            "新建路由".into()
        };
        let frame = workspace.item_window(&title, (490, 344))?;
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
                padding: 10,
                gap: 5,
                ..Layout::default()
            },
        )?;
        let mut layout = Vec::new();
        let enabled = field(
            &general,
            &mut layout,
            "已启用",
            WidgetKind::CheckBox,
            "",
            false,
        )?;
        if !current.is_some_and(|route| route.disabled) {
            enabled.set_state(state::CHECKED)?;
        }
        let destination = field(
            &general,
            &mut layout,
            "目的地址",
            WidgetKind::Edit,
            &prefix,
            false,
        )?;
        let gateway_text = current
            .filter(|route| matches!(route.action, StaticAction::Forward { .. }))
            .map_or_else(String::new, |route| {
                super::catalog::gateway_text(&route.action)
            });
        let gateway = field(
            &general,
            &mut layout,
            "网关",
            WidgetKind::Edit,
            &gateway_text,
            false,
        )?;
        let immediate = ImmediateGateway::new(&general, &mut layout, &[])?;
        let distance = field(
            &general,
            &mut layout,
            "距离",
            WidgetKind::Edit,
            &current.map_or(1, |route| route.preference).to_string(),
            false,
        )?;
        let table = field(
            &general,
            &mut layout,
            "路由表",
            WidgetKind::Combo,
            "",
            false,
        )?;
        table.set_state(state::DROPDOWN_LIST)?;
        let names: Vec<_> = tables.iter().copied().map(table_name).collect();
        table.set_tree_rows(
            &tables
                .iter()
                .zip(&names)
                .map(|(&table, name)| TreeRow {
                    id: u64::from(table),
                    parent: 0,
                    text: name,
                    expanded: false,
                    has_children: false,
                })
                .collect::<Vec<_>>(),
        )?;
        let id = binding.table;
        table.select(Some(u64::from(id)))?;
        let blackhole = field(
            &general,
            &mut layout,
            "黑洞",
            WidgetKind::CheckBox,
            "",
            false,
        )?;
        blackhole.set_state(
            if current.is_some_and(|route| route.action == StaticAction::Discard) {
                state::CHECKED
            } else {
                0
            },
        )?;
        let panel = child(
            &body,
            WidgetKind::Column,
            "",
            Layout {
                width: 84,
                padding: 6,
                ..Layout::default()
            },
        )?;
        let remove = child(&panel, WidgetKind::Link, "× 删除", Layout::default())?;
        panel.set_state(if current.is_some() { 0 } else { state::HIDDEN })?;
        let message = note(&root)?;
        let footer = child(
            &root,
            WidgetKind::Row,
            "",
            Layout {
                gap: 6,
                ..Layout::default()
            },
        )?;
        let status = if family == Family::Ipv4 {
            Some((
                StatusPage::new(&tabs, &mut layout, "静态")?,
                child(&footer, WidgetKind::Label, "", Layout::default())?,
            ))
        } else {
            None
        };
        layout.push(spacer(&footer)?);
        let cancel = child(&footer, WidgetKind::Button, "取消", button())?;
        let apply = child(&footer, WidgetKind::Button, "应用", button())?;
        let ok = child(&footer, WidgetKind::Button, "确定", button())?;
        layout.extend([root, body, tabs, general, footer]);
        let mut editor = Self {
            frame,
            _layout: layout,
            enabled,
            destination,
            gateway,
            immediate,
            members: Vec::new(),
            distance,
            table,
            blackhole,
            cancel,
            apply,
            ok,
            panel,
            remove,
            message,
            status,
            binding,
        };
        editor.ready(true)?;
        editor.show_status(selection)?;
        editor.destination.focus()?;
        Ok(editor)
    }

    pub(super) fn handle_gateway(
        &mut self,
        workspace: &Workspace,
        window: &Window,
        event: &Event,
    ) -> liteos_ui::Result<bool> {
        self.immediate
            .handle(workspace, window, event, &self.members)
    }

    pub(super) fn request(&mut self, event: &Event) -> liteos_ui::Result<Option<Request>> {
        if event.node == self.blackhole.id()
            && matches!(
                event.kind,
                EventKind::Changed { .. } | EventKind::Activate { .. }
            )
        {
            self.ready(true)?;
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(None);
        }
        Ok(
            if event.node == self.frame.id() || event.node == self.cancel.id() {
                Some(Request::Close)
            } else if event.node == self.apply.id() || event.node == self.ok.id() {
                Some(Request::Save {
                    close: event.node == self.ok.id(),
                })
            } else if event.node == self.remove.id() {
                Some(Request::Remove)
            } else {
                None
            },
        )
    }

    pub(super) fn ready(&mut self, ready: bool) -> liteos_ui::Result<()> {
        if !ready {
            self.present_gateway(Vec::new())?;
        }
        let enabled = if ready { 0 } else { state::DISABLED };
        let checked = self.blackhole.state()? & state::CHECKED;
        let terminal = self.binding.original.as_ref().is_some_and(|(_, route)| {
            matches!(
                route.action,
                StaticAction::Unreachable | StaticAction::Prohibit
            )
        });
        for control in [&self.destination, &self.distance, &self.cancel, &self.apply] {
            control.set_state(enabled)?;
        }
        self.enabled
            .set_state((self.enabled.state()? & state::CHECKED) | enabled)?;
        self.ok.set_state(if ready {
            state::DEFAULT
        } else {
            state::DISABLED
        })?;
        self.table.set_state(state::DROPDOWN_LIST | enabled)?;
        self.blackhole
            .set_state(checked | if terminal { state::DISABLED } else { enabled })?;
        self.gateway.set_state(if terminal || checked != 0 {
            state::DISABLED
        } else {
            enabled
        })?;
        self.remove.set_state(enabled)
    }

    /// 保存结果可确认但代际未知时，只允许关闭；不得用事后读出的索引重新绑定草稿。
    pub(super) fn close_only(&mut self) -> liteos_ui::Result<()> {
        self.ready(false)?;
        self.show_status(None)?;
        self.cancel.set_state(0)
    }

    pub(super) fn bind_saved(
        &mut self,
        revision: Revision,
        key: Key,
        route: StaticRoute,
    ) -> liteos_ui::Result<()> {
        self.frame.set_text(&format!(
            "路由 > {}/{} → {}",
            route.destination,
            route.prefix_length,
            super::catalog::gateway_text(&route.action)
        ))?;
        self.binding = Binding {
            revision,
            original: Some((key, route)),
            table: key.table,
        };
        self.present_gateway(Vec::new())?;
        self.show_status(None)?;
        self.panel.set_state(0)
    }

    pub(super) fn present_status(&self, selection: Option<&Selections>) -> liteos_ui::Result<()> {
        let selected = (|| {
            let (key, route) = self.binding.original.as_ref()?;
            let candidate = key.candidate(route)?;
            selection?.get(&(key.table, candidate.location)).copied()
        })();
        self.show_status(selected)
    }

    fn show_status(&self, selection: Option<State>) -> liteos_ui::Result<()> {
        let Some((status, flags)) = &self.status else {
            return Ok(());
        };
        let (contribution, cost, caption) = match selection {
            Some(State::Active(metrics)) => ("活动", Some(metrics.cost), "活动  静态"),
            Some(State::Standby(metrics)) => ("候选", Some(metrics.cost), "静态"),
            Some(State::Unreachable(metrics)) => {
                ("不可达", Some(metrics.cost), "不可达  静态  非活动")
            }
            Some(State::Absent) => ("", None, "静态  非活动"),
            None => (
                "",
                None,
                match &self.binding.original {
                    Some((_, route)) if route.disabled => "停用  静态",
                    Some(_) => "静态",
                    None => "",
                },
            ),
        };
        status.present(contribution, cost)?;
        if flags.text()? != caption {
            flags.set_text(caption)?;
        }
        Ok(())
    }

    fn path(&self) -> Option<(Revision, paths::Request)> {
        let (key, route) = self.binding.original.as_ref()?;
        if route.disabled
            || !matches!(route.action, StaticAction::Forward { .. })
            || self.apply.state().ok()? & state::DISABLED != 0
        {
            return None;
        }
        Some((
            self.binding.revision,
            paths::Request {
                table: key.table,
                candidate: key.candidate(route)?,
            },
        ))
    }

    fn present_gateway(&mut self, members: Vec<NextHop>) -> liteos_ui::Result<()> {
        if self.members != members {
            self.immediate.present(&members)?;
            self.members = members;
        }
        Ok(())
    }

    pub(super) fn note(&self, message: &str) -> liteos_ui::Result<()> {
        set_note(&self.message, message)
    }

    pub(super) fn value(&self, family: Family) -> liteos_ui::Result<Option<(u32, StaticRoute)>> {
        let Ok(prefix) = self.destination.text()?.trim().parse::<IpPrefix>() else {
            return self.invalid(&self.destination, "请输入规范目的网段，例如 192.0.2.0/24。");
        };
        if prefix.network.is_ipv6() != (family == Family::Ipv6) {
            return self.invalid(
                &self.destination,
                "目的地址必须与当前路由列表的地址族一致。",
            );
        }
        let Some(preference) = self
            .distance
            .text()?
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|&n| n > 0)
        else {
            return self.invalid(&self.distance, "距离必须为正整数。");
        };
        let Some(table) = self
            .table
            .selection()?
            .and_then(|id| u32::try_from(id).ok())
        else {
            return self.invalid(&self.table, "请选择有效路由表。");
        };
        let old = self.binding.original.as_ref().map(|(_, route)| route);
        let blackhole = self.blackhole.state()? & state::CHECKED != 0;
        let action = match old.map(|route| &route.action) {
            Some(StaticAction::Unreachable) => StaticAction::Unreachable,
            Some(StaticAction::Prohibit) => StaticAction::Prohibit,
            _ if blackhole => StaticAction::Discard,
            current => {
                let gateway = self.gateway.text()?;
                let members = match current {
                    Some(StaticAction::Forward { members }) => members.as_slice(),
                    _ => &[],
                };
                let Some(members) = collect_members(&gateway, members) else {
                    return self.invalid(
                        &self.gateway,
                        "请输入网关地址、接口名或地址%接口名。多个网关用逗号分隔。",
                    );
                };
                StaticAction::Forward { members }
            }
        };
        Ok(Some((
            table,
            StaticRoute {
                destination: prefix.network,
                prefix_length: prefix.length,
                preference,
                metric: old.map_or(0, |route| route.metric),
                disabled: self.enabled.state()? & state::CHECKED == 0,
                action,
            },
        )))
    }

    fn invalid<T>(&self, field: &Widget, reason: &str) -> liteos_ui::Result<Option<T>> {
        self.note(reason)?;
        field.focus()?;
        Ok(None)
    }
}

pub(super) async fn observe_gateway(
    view: Rc<RefCell<View>>,
    frame: u64,
    background: Background,
    clock: Clock,
) {
    loop {
        let (path, scope) = {
            let view = view.borrow();
            let Some((_, editor)) = view
                .editor
                .as_ref()
                .filter(|(_, editor)| editor.frame.id() == frame)
            else {
                return;
            };
            (editor.path(), view.scope.clone())
        };
        if let Some((revision, request)) = path {
            let result = match background
                .run(move || ManagementClient::local().static_paths(scope.as_deref(), request))
            {
                Ok(task) => task
                    .await
                    .ok()
                    .filter(|(observed, _)| *observed == revision),
                Err(_) => None,
            };
            let mut view = view.borrow_mut();
            let Some((_, editor)) = view
                .editor
                .as_mut()
                .filter(|(_, editor)| editor.frame.id() == frame)
            else {
                return;
            };
            if editor.path() == path {
                let members = result.map(|(_, members)| members).unwrap_or_default();
                if editor.present_gateway(members).is_err() {
                    return;
                }
            }
        }
        let Ok(delay) = clock.sleep(Duration::from_secs(2)) else {
            return;
        };
        delay.await;
    }
}

fn collect_members(text: &str, existing: &[StaticMember]) -> Option<Vec<StaticMember>> {
    // 保留原声明的权重和 on-link 标记；这些属性没有对应的 WinBox 表单字段。
    let labels: Vec<_> = existing
        .iter()
        .map(|member| target_text(&member.target))
        .collect();
    if !existing.is_empty() && labels.join(", ") == text {
        return Some(existing.to_vec());
    }
    text.split(',')
        .map(|part| {
            let part = part.trim();
            if let Some(index) = labels.iter().position(|label| label == part) {
                return Some(existing[index].clone());
            }
            let target = if let Ok(address) = part.parse::<IpAddr>() {
                StaticTarget::Recursive { address }
            } else {
                let (gateway, interface) = if let Some((address, interface)) = part.split_once('%')
                    && let Ok(address) = address.parse::<IpAddr>()
                {
                    (Some(address), interface)
                } else {
                    (None, part)
                };
                StaticTarget::Direct {
                    interface: ConfigurationName::try_from(interface.to_owned()).ok()?,
                    gateway,
                    on_link: false,
                }
            };
            Some(StaticMember { weight: 1, target })
        })
        .collect()
}
