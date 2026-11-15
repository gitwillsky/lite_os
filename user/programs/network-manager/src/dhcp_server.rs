//! WinBox 的 DHCP 服务器工作区。配置与租约分别来自配置 owner 和活动的 DHCP 会话。

mod catalog;
mod dialog;

use self::{
    catalog::{Catalog, Lease},
    dialog::Pane,
};
use crate::{
    child,
    configuration::EditTransaction,
    item_list::{Glyph, Item, Table, Tool},
    item_properties::{button, field},
    workspace::Workspace,
};
use liteos_ui::{
    Application, Background, Event, EventKind, Layout, MessageButtons, MessageIcon, MessageResult,
    Task, Widget, WidgetKind, Window, state,
};
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

const TITLE: &str = "DHCP 服务器";
const POLL_PERIOD: Duration = Duration::from_secs(2);

pub(super) struct Page {
    view: Rc<View>,
    background: Background,
    scope: Option<Arc<str>>,
    visible: bool,
    observer: Option<Task<()>>,
    editor: Option<Task<()>>,
}

struct View {
    tabs: Widget,
    dhcp: Table,
    networks: Table,
    leases: Table,
    setup: Tool,
    remove: Tool,
    catalog: RefCell<Option<Catalog>>,
    active: RefCell<Vec<Lease>>,
    property: RefCell<Option<LeaseProperty>>,
}

impl Page {
    /// 在工作区建立 DHCP、网络和租约页，窗口打开时才读取本域。
    /// 错误：父窗口已关闭或控件创建失败。
    pub(super) fn new(parent: &Widget, background: Background) -> liteos_ui::Result<Self> {
        let tabs = child(
            parent,
            WidgetKind::Tabs,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        let (dhcp, [remove, setup]) = Table::new(
            &tabs,
            "DHCP",
            &[("接口", 160), ("租期", 105), ("地址池", 290)],
            [
                Item::Command(Glyph::Remove, "删除"),
                Item::Text("DHCP 设置向导…"),
            ],
        )?;
        let (networks, []) = Table::new(
            &tabs,
            "网络",
            &[("地址", 150), ("网关", 150), ("DNS 服务器", 280)],
            [],
        )?;
        let (leases, []) = Table::new(
            &tabs,
            "租约",
            &[
                ("", 34),
                ("地址", 132),
                ("MAC 地址", 154),
                ("服务器", 145),
                ("到期时间", 95),
                ("状态", 68),
            ],
            [],
        )?;
        tabs.select(Some(dhcp.pane.id()))?;
        remove.set_enabled(false)?;
        setup.set_enabled(false)?;
        Ok(Self {
            view: Rc::new(View {
                tabs,
                dhcp,
                networks,
                leases,
                remove,
                setup,
                catalog: RefCell::new(None),
                active: RefCell::default(),
                property: RefCell::default(),
            }),
            background,
            scope: None,
            visible: false,
            observer: None,
            editor: None,
        })
    }

    /// 显示或隐藏 DHCP 工作区，隐藏时结束观察。
    /// 错误：窗口已关闭或观察任务无法启动。
    pub(super) fn show(
        &mut self,
        app: &mut Application,
        window: &Window,
        open: bool,
    ) -> liteos_ui::Result<()> {
        if self.visible != open {
            self.visible = open;
            self.observe(app, window)?;
        }
        Ok(())
    }

    /// 切换路由域，立即清除上一个域的显示和选区。
    /// 错误：窗口已关闭或观察任务无法启动。
    pub(super) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        if self.scope != domain {
            self.scope = domain;
            for pane in [Pane::Dhcp, Pane::Networks, Pane::Leases] {
                let table = self.view.table(pane);
                table.replace(Vec::new(), None)?;
                table.clear_find()?;
            }
            self.observe(app, window)?;
        }
        Ok(())
    }

    fn observe(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        self.observer = None;
        self.view.catalog.borrow_mut().take();
        self.view.active.borrow_mut().clear();
        self.view.property.borrow_mut().take();
        let pane = self.view.selected()?;
        let table = self.view.table(pane);
        table.replace(Vec::new(), None)?;
        self.view.setup.set_enabled(false)?;
        self.view.remove.set_enabled(false)?;
        if !self.visible {
            return Ok(());
        }
        table.summary.notice("正在读取…")?;
        let scope = self.scope.clone();
        let view = self.view.clone();
        let background = self.background.clone();
        let clock = app.clock();
        self.observer = Some(app.spawn_local(window, async move {
            loop {
                let domain = scope.clone();
                let result = if pane == Pane::Leases {
                    match background.run(move || catalog::leases(domain.as_deref())) {
                        Ok(task) => view.leases_result(task.await),
                        Err(error) => view.failed(pane, &error.to_string()),
                    }
                } else {
                    let current = view
                        .catalog
                        .borrow()
                        .as_ref()
                        .map(|current| current.baseline.revision);
                    match background.run(move || Catalog::load(domain.as_deref(), current)) {
                        Ok(task) => view.catalog_result(pane, task.await),
                        Err(error) => view.failed(pane, &error.to_string()),
                    }
                };
                if result.is_err() {
                    return;
                }
                let Ok(next) = clock.sleep(POLL_PERIOD) else {
                    return;
                };
                next.await;
            }
        })?);
        Ok(())
    }

    /// 分派当前页签的命令、列表选择和属性操作。
    /// 错误：窗口关闭、读取选区失败或操作任务无法启动。
    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        workspace: &Workspace,
        event: &Event,
    ) -> liteos_ui::Result<()> {
        if !self.visible {
            return Ok(());
        }
        if event.node == self.view.tabs.id() && matches!(event.kind, EventKind::Select { .. }) {
            return self.observe(app, window);
        }
        let pane = self.view.selected()?;
        let table = self.view.table(pane);
        table.handle_find(event)?;
        if event.node == table.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            table.selection_changed()?;
            if pane == Pane::Dhcp {
                self.view.remove.set_enabled(table.selection()?.is_some())?;
            }
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        if pane == Pane::Leases {
            let mut property = self.view.property.borrow_mut();
            if property.as_ref().is_some_and(|shown| shown.closed(event)) {
                property.take();
            } else if event.node == table.list.id()
                && let Some(row) = table
                    .selection()?
                    .and_then(|index| self.view.active.borrow().get(index).cloned())
            {
                if let Some(shown) = &*property
                    && shown.same(&row)
                {
                    workspace.raise(&shown.frame)?;
                } else {
                    *property = Some(LeaseProperty::new(workspace, row)?);
                }
            }
            return Ok(());
        }
        if self.editor.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        let catalog = self.view.catalog.borrow();
        let Some(catalog) = catalog.as_ref() else {
            return Ok(());
        };
        let targets = &catalog.targets;
        let selected = table.selection()?.and_then(|index| {
            targets
                .iter()
                .filter(|server| server.pool.is_some())
                .nth(index)
        });
        let operation = if pane == Pane::Dhcp && self.view.setup.activated(event) {
            Operation::Setup(targets.clone())
        } else if let Some(server) = selected
            && (event.node == table.list.id()
                || pane == Pane::Dhcp && self.view.remove.activated(event))
        {
            if event.node == table.list.id() {
                Operation::Edit(server.clone(), pane)
            } else {
                Operation::Remove(server.interface.clone())
            }
        } else {
            return Ok(());
        };
        let mut transaction = EditTransaction::new(
            catalog.baseline.clone(),
            self.scope.clone(),
            self.background.clone(),
        );
        let dialogs = app.dialogs();
        let parent = window.clone();
        self.editor = Some(app.spawn_local(window, async move {
            let result = async {
                match operation {
                    Operation::Setup(choices) => {
                        dialog::setup(&dialogs, &parent, &choices, transaction).await
                    }
                    Operation::Edit(server, pane) => {
                        dialog::edit(&dialogs, &parent, &server, pane, transaction).await
                    }
                    Operation::Remove(interface) => {
                        if dialogs
                            .message(
                                &parent,
                                TITLE,
                                &format!("删除 {} 上的 DHCP 服务器？", interface.as_str()),
                                MessageButtons::YesNo,
                                MessageIcon::Question,
                            )?
                            .await?
                            == MessageResult::Yes
                        {
                            dialog::save_pool(&mut transaction, interface, None).await?;
                        }
                        Ok(())
                    }
                }
            }
            .await;
            if let Err(error) = result
                && let Ok(alert) = dialogs.message(
                    &parent,
                    TITLE,
                    &error.to_string(),
                    MessageButtons::Ok,
                    MessageIcon::Error,
                )
            {
                let _ = alert.await;
            }
        })?);
        Ok(())
    }
}

enum Operation {
    Setup(Vec<catalog::Server>),
    Edit(catalog::Server, Pane),
    Remove(network_config::configuration::ConfigurationName),
}

impl View {
    fn selected(&self) -> liteos_ui::Result<Pane> {
        let id = self.tabs.selection()?;
        Ok(if id == Some(self.networks.pane.id()) {
            Pane::Networks
        } else if id == Some(self.leases.pane.id()) {
            Pane::Leases
        } else {
            Pane::Dhcp
        })
    }

    fn table(&self, pane: Pane) -> &Table {
        match pane {
            Pane::Dhcp => &self.dhcp,
            Pane::Networks => &self.networks,
            Pane::Leases => &self.leases,
        }
    }

    fn catalog_result(
        &self,
        pane: Pane,
        next: std::io::Result<Option<Catalog>>,
    ) -> liteos_ui::Result<()> {
        let catalog = match next {
            Ok(Some(catalog)) => catalog,
            Ok(None) => return Ok(()),
            Err(error) => return self.failed(pane, &error.to_string()),
        };
        let table = self.table(pane);
        let mut previous = self.catalog.borrow_mut();
        let selected = table.selection()?.and_then(|index| {
            previous
                .as_ref()?
                .targets
                .iter()
                .filter(|server| server.pool.is_some())
                .nth(index)
                .map(|server| &server.interface)
        });
        let mut next_selection = None;
        let rows = catalog
            .targets
            .iter()
            .filter_map(|entry| entry.pool.as_ref().map(|pool| (entry, pool)))
            .enumerate()
            .map(|(index, (server, pool))| {
                if selected == Some(&server.interface) {
                    next_selection = Some(index);
                }
                if pane == Pane::Dhcp {
                    format!(
                        "{}\t{}\t{}-{}",
                        server.interface.as_str(),
                        duration(Duration::from_secs(u64::from(pool.lease_seconds))),
                        pool.first,
                        pool.last
                    )
                } else {
                    let gateway = pool
                        .router
                        .map_or_else(String::new, |address| address.to_string());
                    let dns = pool
                        .dns
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{}\t{gateway}\t{dns}", server.network())
                }
            })
            .collect();
        table.replace(rows, next_selection)?;
        if pane == Pane::Dhcp {
            self.setup.set_enabled(true)?;
            self.remove.set_enabled(next_selection.is_some())?;
        }
        *previous = Some(catalog);
        Ok(())
    }

    fn leases_result(&self, next: std::io::Result<Vec<Lease>>) -> liteos_ui::Result<()> {
        let active = match next {
            Ok(rows) => rows,
            Err(error) => return self.failed(Pane::Leases, &error.to_string()),
        };
        let mut previous = self.active.borrow_mut();
        let selected = self
            .leases
            .selection()?
            .and_then(|index| previous.get(index))
            .and_then(|old| active.iter().position(|row| same_binding(row, old)));
        let rows = active
            .iter()
            .map(|lease| {
                format!(
                    "D\t{}\t{}\t{}\t{}\tbound",
                    lease.address,
                    lease.mac,
                    lease.interface,
                    expiry(lease),
                )
            })
            .collect();
        self.leases.replace(rows, selected)?;
        let mut property = self.property.borrow_mut();
        if let Some(shown) = &*property {
            if let Some(lease) = active.iter().find(|lease| shown.same(lease)) {
                shown.remaining.set_text(&expiry(lease))?;
            } else {
                property.take();
            }
        }
        *previous = active;
        Ok(())
    }

    fn failed(&self, pane: Pane, message: &str) -> liteos_ui::Result<()> {
        let table = self.table(pane);
        table.replace(Vec::new(), None)?;
        table.summary.notice(message)?;
        self.setup.set_enabled(false)?;
        self.remove.set_enabled(false)?;
        self.active.borrow_mut().clear();
        self.catalog.borrow_mut().take();
        self.property.borrow_mut().take();
        Ok(())
    }
}

struct LeaseProperty {
    frame: Widget,
    ok: Widget,
    remaining: Widget,
    lease: Lease,
    _decor: Vec<Widget>,
}

impl LeaseProperty {
    fn new(workspace: &Workspace, lease: Lease) -> liteos_ui::Result<Self> {
        let frame = workspace.item_window(&format!("DHCP 租约 <{}>", lease.address), (484, 228))?;
        let root = child(
            &frame,
            WidgetKind::Row,
            "",
            Layout {
                grow: 1,
                gap: 8,
                padding: 8,
                ..Layout::default()
            },
        )?;
        let tabs = child(
            &root,
            WidgetKind::Tabs,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        let mut decor = Vec::new();
        let pane = |title| {
            child(
                &tabs,
                WidgetKind::Column,
                title,
                Layout {
                    grow: 1,
                    padding: 5,
                    gap: 5,
                    ..Layout::default()
                },
            )
        };
        let general = pane("常规")?;
        let active = pane("活动")?;
        for (target, fields) in [
            (&general, ["地址", "MAC 地址", "服务器"]),
            (&active, ["活动地址", "活动 MAC 地址", "活动服务器"]),
        ] {
            for (label, value) in fields.into_iter().zip([
                lease.address.to_string().as_str(),
                &lease.mac,
                &lease.interface,
            ]) {
                let entry = field(target, &mut decor, label, WidgetKind::Edit, value, true)?;
                decor.push(entry);
            }
        }
        let remaining = field(
            &active,
            &mut decor,
            "到期时间",
            WidgetKind::Edit,
            &expiry(&lease),
            true,
        )?;
        let ok = child(&root, WidgetKind::Button, "确定", button())?;
        ok.set_state(state::DEFAULT)?;
        tabs.select(Some(active.id()))?;
        decor.extend([root, tabs, general, active]);
        Ok(Self {
            frame,
            ok,
            remaining,
            lease,
            _decor: decor,
        })
    }

    fn same(&self, lease: &Lease) -> bool {
        same_binding(&self.lease, lease)
    }

    fn closed(&self, event: &Event) -> bool {
        [self.ok.id(), self.frame.id()].contains(&event.node)
    }
}

fn same_binding(left: &Lease, right: &Lease) -> bool {
    (left.address, &left.interface, &left.mac) == (right.address, &right.interface, &right.mac)
}

fn expiry(lease: &Lease) -> String {
    lease.remaining.map_or_else(|| "永久".to_owned(), duration)
}

fn duration(value: Duration) -> String {
    let seconds = value.as_millis().div_ceil(1000);
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}
