//! WinBox IPv6 ND 工作区。接口管理通告策略，前缀页展示由静态接口地址生成的前缀。

mod catalog;
mod property;

use self::{catalog::Catalog, property::PrefixProperty};
use crate::{
    child,
    configuration::EditTransaction,
    item_list::{Glyph, Item, Table, Tool},
    workspace::Workspace,
};
use liteos_ui::{
    Application, Background, Event, EventKind, Layout, MessageButtons, MessageIcon, MessageResult,
    Task, Widget, WidgetKind, Window,
};
use network_config::configuration::RouterAdvertisements;
use std::{cell::RefCell, io, rc::Rc, sync::Arc, time::Duration};

const TITLE: &str = "邻居发现";

pub(super) struct Page {
    view: Rc<View>,
    scope: Option<Arc<str>>,
    visible: bool,
    background: Background,
    observer: Option<Task<()>>,
    editor: Option<Task<()>>,
}

struct View {
    tabs: Widget,
    interfaces: Table,
    prefixes: Table,
    add: Tool,
    remove: Tool,
    catalog: RefCell<Option<Catalog>>,
    property: RefCell<Option<PrefixProperty>>,
}

impl Page {
    /// 创建含接口和前缀页签的 ND 工作区。错误：控件无法创建或父窗口已关闭。
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
        let (interfaces, [add, remove]) = Table::new(
            &tabs,
            "接口",
            &[
                ("接口", 149),
                ("RA 最大间隔", 105),
                ("RA 寿命", 100),
                ("通告 DNS", 91),
                ("DNS", 202),
            ],
            [
                Item::Command(Glyph::Add, "新建"),
                Item::Command(Glyph::Remove, "删除"),
            ],
        )?;
        let (prefixes, []) = Table::new(
            &tabs,
            "前缀",
            &[
                ("", 37),
                ("前缀", 266),
                ("接口", 170),
                ("链路上", 72),
                ("自治", 72),
            ],
            [],
        )?;
        tabs.select(Some(interfaces.pane.id()))?;
        add.set_enabled(false)?;
        remove.set_enabled(false)?;
        Ok(Self {
            view: Rc::new(View {
                tabs,
                interfaces,
                prefixes,
                add,
                remove,
                catalog: RefCell::default(),
                property: RefCell::default(),
            }),
            scope: None,
            visible: false,
            background,
            observer: None,
            editor: None,
        })
    }

    /// 仅在窗口打开时读取和更新。错误：窗口关闭或无法启动查询任务。
    pub(super) fn show(
        &mut self,
        app: &mut Application,
        window: &Window,
        visible: bool,
    ) -> liteos_ui::Result<()> {
        if self.visible != visible {
            self.visible = visible;
            self.observe(app, window)?;
        }
        Ok(())
    }

    /// 更换查询范围并清除上一域的选区和属性。错误：窗口关闭或任务无法启动。
    pub(super) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        scope: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        if self.scope != scope {
            self.scope = scope;
            self.view.interfaces.clear_find()?;
            self.view.prefixes.clear_find()?;
            self.observe(app, window)?;
        }
        Ok(())
    }

    fn observe(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        self.observer = None;
        self.view.catalog.borrow_mut().take();
        self.view.property.borrow_mut().take();
        self.view.clear("正在读取…")?;
        if !self.visible {
            return Ok(());
        }
        let view = self.view.clone();
        let scope = self.scope.clone();
        let background = self.background.clone();
        let clock = app.clock();
        self.observer = Some(app.spawn_local(window, async move {
            loop {
                let current = view
                    .catalog
                    .borrow()
                    .as_ref()
                    .map(|catalog| catalog.baseline.revision);
                let target = scope.clone();
                let result = match background.run(move || Catalog::load(target.as_deref(), current))
                {
                    Ok(task) => task.await,
                    Err(error) => Err(io::Error::other(error.to_string())),
                };
                let updated = match result {
                    Ok(Some(catalog)) => view.publish(catalog),
                    Ok(None) => Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(()),
                    Err(error) => {
                        view.catalog.borrow_mut().take();
                        view.property.borrow_mut().take();
                        view.clear(&error.to_string())
                    }
                };
                if updated.is_err() {
                    return;
                }
                let Ok(next) = clock.sleep(Duration::from_secs(2)) else {
                    return;
                };
                next.await;
            }
        })?);
        Ok(())
    }

    /// 分派查找、新建、打开属性和删除。错误：工作区已关闭或无法启动属性任务。
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
        if self
            .view
            .property
            .borrow()
            .as_ref()
            .is_some_and(|property| property.closed(event))
        {
            self.view.property.borrow_mut().take();
            return Ok(());
        }
        let prefixes = self.view.tabs.selection()? == Some(self.view.prefixes.pane.id());
        let table = if prefixes {
            &self.view.prefixes
        } else {
            &self.view.interfaces
        };
        table.handle_find(event)?;
        if event.node == table.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            table.selection_changed()?;
            if !prefixes {
                self.view.remove.set_enabled(table.selection()?.is_some())?;
            }
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        let catalog = self.view.catalog.borrow();
        let Some(catalog) = catalog.as_ref() else {
            return Ok(());
        };
        let selected = table.selection()?;
        if prefixes {
            if event.node == table.list.id()
                && let Some(prefix) = selected.and_then(|index| catalog.prefixes.get(index))
            {
                let mut property = self.view.property.borrow_mut();
                if let Some(shown) = &*property
                    && shown.prefix == *prefix
                {
                    workspace.raise(&shown.frame)?;
                } else {
                    *property = Some(PrefixProperty::open(workspace, prefix.clone())?);
                }
            }
            return Ok(());
        }
        if self.editor.as_ref().is_some_and(|job| !job.is_finished()) {
            return Ok(());
        }
        let selected = selected.and_then(|index| catalog.configured().nth(index));
        let operation = if self.view.add.activated(event) {
            Operation::Edit(None)
        } else if let Some(interface) = selected
            && self.view.remove.activated(event)
        {
            Operation::Remove(interface.clone())
        } else if event.node == table.list.id()
            && let Some(interface) = selected
        {
            Operation::Edit(
                catalog
                    .interfaces
                    .iter()
                    .position(|entry| entry.binding == interface.binding),
            )
        } else {
            return Ok(());
        };
        let mut transaction = EditTransaction::new(
            catalog.baseline.clone(),
            self.scope.clone(),
            self.background.clone(),
        );
        let choices = catalog.interfaces.clone();
        let dialogs = app.dialogs();
        let parent = window.clone();
        self.editor = Some(app.spawn_local(window, async move {
            let result = async {
                match operation {
                    Operation::Edit(index) => {
                        property::edit(&dialogs, &parent, choices, index, transaction).await
                    }
                    Operation::Remove(interface) => {
                        if dialogs
                            .message(
                                &parent,
                                TITLE,
                                &format!("删除 {} 的 ND 配置？", interface.name),
                                MessageButtons::YesNo,
                                MessageIcon::Question,
                            )?
                            .await?
                            == MessageResult::Yes
                        {
                            transaction
                                .save(move |config, scope| {
                                    catalog::set(
                                        config,
                                        scope,
                                        &interface.binding,
                                        RouterAdvertisements::Disabled,
                                    )
                                })
                                .await?;
                        }
                        Ok(())
                    }
                }
            }
            .await;
            if let Err(error) = result
                && let Ok(message) = dialogs.message(
                    &parent,
                    TITLE,
                    &error.to_string(),
                    MessageButtons::Ok,
                    MessageIcon::Error,
                )
            {
                let _ = message.await;
            }
        })?);
        Ok(())
    }
}

enum Operation {
    Edit(Option<usize>),
    Remove(catalog::Interface),
}

impl View {
    fn clear(&self, message: &str) -> liteos_ui::Result<()> {
        for table in [&self.interfaces, &self.prefixes] {
            table.replace(Vec::new(), None)?;
            table.summary.notice(message)?;
        }
        self.add.set_enabled(false)?;
        self.remove.set_enabled(false)
    }

    fn publish(&self, catalog: Catalog) -> liteos_ui::Result<()> {
        let mut previous = self.catalog.borrow_mut();
        let selected = self
            .interfaces
            .selection()?
            .and_then(|index| previous.as_ref()?.configured().nth(index))
            .and_then(|old| {
                catalog
                    .configured()
                    .position(|entry| entry.binding == old.binding)
            });
        let interfaces = catalog
            .configured()
            .filter_map(|entry| {
                let RouterAdvertisements::Enabled {
                    max_interval_seconds,
                    router_lifetime_seconds,
                    dns,
                } = entry.policy
                else {
                    return None;
                };
                let values = dns
                    .as_slice()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!(
                    "{}\t{} s\t{} s\t{}\t{values}",
                    entry.name,
                    max_interval_seconds,
                    router_lifetime_seconds,
                    if values.is_empty() { "否" } else { "是" }
                ))
            })
            .collect();
        self.interfaces.replace(interfaces, selected)?;
        if !catalog.forwarding {
            // 已配置的条目仍可管理，但发送端要求本域 IPv6 转发开启。
            self.interfaces.summary.show(
                catalog.configured().count(),
                usize::from(selected.is_some()),
                "IPv6 转发已关闭",
            )?;
        }
        let prefix_selection = self
            .prefixes
            .selection()?
            .and_then(|index| previous.as_ref()?.prefixes.get(index))
            .and_then(|old| catalog.prefixes.iter().position(|prefix| prefix == old));
        let rows = catalog
            .prefixes
            .iter()
            .map(|prefix| format!("D\t{}/64\t{}\t是\t是", prefix.network, prefix.interface))
            .collect();
        self.prefixes.replace(rows, prefix_selection)?;
        if self
            .property
            .borrow()
            .as_ref()
            .is_some_and(|p| !catalog.prefixes.contains(&p.prefix))
        {
            self.property.borrow_mut().take();
        }
        self.add.set_enabled(
            catalog
                .interfaces
                .iter()
                .any(|entry| entry.policy == RouterAdvertisements::Disabled),
        )?;
        self.remove.set_enabled(selected.is_some())?;
        *previous = Some(catalog);
        Ok(())
    }
}
