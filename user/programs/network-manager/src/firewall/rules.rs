//! 过滤和 NAT 共用列表、只读计数采样与按代际提交。

use super::catalog::{self, Catalog, Change, Counts, Entry, Scope};
use crate::{
    child,
    configuration::apply,
    item_list::{Glyph, Item, Summary, Tool, toolbar},
    routes::{edit_rule, rule_address},
};
use liteos_ui::{
    Application, Background, Clock, Column, Dialogs, Event, EventKind, Layout, ListOptions,
    ListStyle, MessageButtons, MessageIcon, MessageResult, Task, Widget, WidgetKind, Window, state,
};
use network_config::{configuration::PreroutingRule, firewall::configured_action};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

const PAGE_ROWS: usize = network_config::firewall::counters::LIMIT;
type Error = Box<dyn std::error::Error>;

pub(super) struct Page {
    view: Rc<View>,
    domain: Option<Arc<str>>,
    observation: Option<Task<()>>,
    mutation: Option<Task<()>>,
    visible: bool,
}

struct Sample {
    catalog: Arc<Catalog>,
    start: usize,
    counts: Counts,
}

struct View {
    scope: Scope,
    list: Widget,
    new: Tool,
    enable: Tool,
    disable: Tool,
    remove: Tool,
    paging: Widget,
    previous: Tool,
    next: Tool,
    refresh: Tool,
    summary: Summary,
    sample: RefCell<Option<Sample>>,
    // 对话框与提交共用操作期，防止异步采样重开按钮。
    editing: Cell<bool>,
    background: Background,
    clock: Clock,
    dialogs: Dialogs,
}

enum Request {
    Edit(Option<usize>),
    Enable(usize, bool),
    Remove(usize),
}

impl Page {
    pub(super) fn new(
        app: &Application,
        parent: &Widget,
        background: Background,
        scope: Scope,
    ) -> liteos_ui::Result<Self> {
        let [new, enable, disable, remove] = toolbar(
            parent,
            [
                Item::Command(Glyph::Add, "新建"),
                Item::Command(Glyph::Enable, "启用"),
                Item::Command(Glyph::Disable, "停用"),
                Item::Command(Glyph::Remove, "删除"),
            ],
        )?;
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
        let columns = [
            ("#", 40),
            ("标志", 45),
            ("动作", 136),
            ("规则链", 100),
            ("源地址", 117),
            ("目的地址", 117),
            ("协议", 60),
            ("目的端口", 72),
            ("入接口", 90),
            ("字节数", 68),
            ("报文数", 64),
        ];
        let columns: Vec<_> = columns
            .into_iter()
            .enumerate()
            .map(|(index, (title, width))| Column {
                id: index as u64 + 1,
                title,
                width,
            })
            .collect();
        list.set_columns(&columns)?;
        let paging = child(parent, WidgetKind::Column, "", Layout::default())?;
        let [previous, next, refresh] = toolbar(
            &paging,
            [
                Item::Icon(Glyph::Previous, "上一页"),
                Item::Icon(Glyph::Next, "下一页"),
                Item::Icon(Glyph::Refresh, "刷新"),
            ],
        )?;
        paging.set_state(state::HIDDEN)?;
        let view = Rc::new(View {
            scope,
            list,
            new,
            enable,
            disable,
            remove,
            paging,
            previous,
            next,
            refresh,
            summary: Summary::new(parent)?,
            sample: RefCell::default(),
            editing: Cell::new(false),
            background,
            clock: app.clock(),
            dialogs: app.dialogs(),
        });
        view.buttons()?;
        Ok(Self {
            view,
            domain: None,
            observation: None,
            mutation: None,
            visible: false,
        })
    }

    pub(super) fn show(
        &mut self,
        app: &mut Application,
        window: &Window,
        visible: bool,
    ) -> liteos_ui::Result<()> {
        if self.visible == visible {
            return Ok(());
        }
        self.visible = visible;
        if visible {
            self.subscribe(app, window, 0)?;
        } else {
            self.observation = None;
        }
        Ok(())
    }

    pub(super) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        self.domain = domain;
        self.view.clear()?;
        if self.visible {
            self.subscribe(app, window, 0)?;
        }
        Ok(())
    }

    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        event: &Event,
    ) -> liteos_ui::Result<()> {
        if !self.visible {
            return Ok(());
        }
        let view = &self.view;
        if event.node == view.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            view.summary()?;
            return view.buttons();
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        if view.refresh.activated(event) {
            return self.subscribe(app, window, 0);
        }
        let Some(start) = view.sample.borrow().as_ref().map(|sample| sample.start) else {
            return Ok(());
        };
        if view.previous.activated(event) || view.next.activated(event) {
            let start = if view.previous.activated(event) {
                start.saturating_sub(PAGE_ROWS)
            } else {
                start + PAGE_ROWS
            };
            return self.subscribe(app, window, start);
        }
        if view.editing.get() {
            return Ok(());
        }
        let request = if view.new.activated(event) {
            Request::Edit(None)
        } else if let Some(index) = view.selected()? {
            if event.node == view.list.id() {
                Request::Edit(Some(index))
            } else if view.enable.activated(event) || view.disable.activated(event) {
                Request::Enable(index, view.enable.activated(event))
            } else if view.remove.activated(event) {
                Request::Remove(index)
            } else {
                return Ok(());
            }
        } else {
            return Ok(());
        };
        let catalog = view
            .sample
            .borrow()
            .as_ref()
            .expect("ready sample")
            .catalog
            .clone();
        view.editing.set(true);
        view.buttons()?;
        let scope = self.domain.clone();
        let parent = window.clone();
        let view = view.clone();
        self.mutation = Some(app.spawn_local(window, async move {
            let result = view.mutate(&parent, scope, catalog, request).await;
            if let Err(error) = result {
                let _ = view.report(&parent, &error.to_string()).await;
            }
            view.editing.set(false);
            let _ = view.buttons();
        })?);
        Ok(())
    }

    fn subscribe(
        &mut self,
        app: &mut Application,
        window: &Window,
        start: usize,
    ) -> liteos_ui::Result<()> {
        self.observation = None;
        let view = self.view.clone();
        let catalog = view
            .sample
            .borrow()
            .as_ref()
            .map(|sample| sample.catalog.clone());
        view.clear()?;
        view.refresh.set_enabled(false)?;
        view.summary.notice("正在读取规则…")?;
        let domain = self.domain.clone();
        self.observation = Some(app.spawn_local(window, async move {
            if let Err(error) = view.observe(domain, catalog, start).await {
                let _ = view.clear();
                let _ = view.paging.set_state(0);
                let _ = view.refresh.set_enabled(true);
                let _ = view.summary.notice(&format!("无法读取防火墙规则。{error}"));
            }
        })?);
        Ok(())
    }
}

impl View {
    async fn observe(
        &self,
        domain: Option<Arc<str>>,
        mut catalog: Option<Arc<Catalog>>,
        mut start: usize,
    ) -> Result<(), Error> {
        loop {
            let domain = domain.clone();
            let scope = self.scope;
            let previous = catalog.take();
            let (next, offset, counts) = self
                .background
                .run(move || Catalog::sample(domain.as_deref(), scope, previous, start))?
                .await?;
            start = offset;
            let (new_list, changed) = {
                let previous = self.sample.borrow();
                let new_list = previous.as_ref().is_none_or(|sample| {
                    sample.catalog.revision != next.revision || sample.start != start
                });
                let changed = new_list
                    || previous
                        .as_ref()
                        .is_some_and(|sample| sample.counts != counts);
                (new_list, changed)
            };
            catalog = Some(next.clone());
            if changed {
                *self.sample.borrow_mut() = Some(Sample {
                    catalog: next,
                    start,
                    counts,
                });
                self.present(new_list)?;
            }
            self.clock.sleep(Duration::from_secs(1))?.await;
        }
    }

    fn selected(&self) -> liteos_ui::Result<Option<usize>> {
        let id = self.list.selection()?;
        let sample = self.sample.borrow();
        Ok(sample.as_ref().and_then(|sample| {
            let index = usize::try_from(id?.checked_sub(1)?).ok()?;
            (index >= sample.start && index < sample.start + sample.counts.len()).then_some(index)
        }))
    }

    fn buttons(&self) -> liteos_ui::Result<()> {
        let selected = self.selected()?;
        let sample = self.sample.borrow();
        let ready = sample.is_some() && !self.editing.get();
        let rule = sample
            .as_ref()
            .and_then(|sample| sample.catalog.rows.get(selected?));
        self.new.set_enabled(ready)?;
        self.enable
            .set_enabled(ready && rule.is_some_and(|entry| entry.rule.disabled))?;
        self.disable
            .set_enabled(ready && rule.is_some_and(|entry| !entry.rule.disabled))?;
        self.remove.set_enabled(ready && rule.is_some())?;
        let previous = sample.as_ref().is_some_and(|sample| sample.start != 0);
        let next = sample
            .as_ref()
            .is_some_and(|sample| sample.start + PAGE_ROWS < sample.catalog.rows.len());
        self.previous.set_enabled(previous)?;
        self.next.set_enabled(next)?;
        self.paging
            .set_state(if previous || next { 0 } else { state::HIDDEN })
    }

    fn clear(&self) -> liteos_ui::Result<()> {
        self.sample.borrow_mut().take();
        self.list.set_rows(&[])?;
        self.buttons()
    }

    fn summary(&self) -> liteos_ui::Result<()> {
        if let Some(sample) = self.sample.borrow().as_ref() {
            self.summary.show(
                sample.counts.len(),
                usize::from(self.list.selection()?.is_some()),
                "实时",
            )?;
        }
        Ok(())
    }

    fn present(&self, new_list: bool) -> liteos_ui::Result<()> {
        if new_list {
            self.list.select(None)?;
        }
        let sample = self.sample.borrow();
        let sample = sample.as_ref().expect("ready sample");
        let text: Vec<_> = sample.catalog.rows[sample.start..]
            .iter()
            .zip(&sample.counts)
            .map(|(entry, &counts)| row_text(entry, counts))
            .collect();
        let rows: Vec<_> = text
            .iter()
            .enumerate()
            .map(|(index, text)| ((sample.start + index) as u64 + 1, text.as_str()))
            .collect();
        self.list.set_rows(&rows)?;
        self.summary()?;
        self.buttons()
    }

    async fn mutate(
        &self,
        window: &Window,
        domain: Option<Arc<str>>,
        catalog: Arc<Catalog>,
        request: Request,
    ) -> Result<(), Error> {
        let selected = match request {
            Request::Edit(index) => index,
            Request::Enable(index, _) | Request::Remove(index) => Some(index),
        }
        .map(|index| &catalog.rows[index]);
        if selected.is_some_and(|entry| rule_address(&entry.rule).is_none())
            && self
                .dialogs
                .message(
                    window,
                    "防火墙规则",
                    "这条规则同时匹配 IPv4 和 IPv6。继续操作会影响两个地址族。",
                    MessageButtons::YesNo,
                    MessageIcon::Warning,
                )?
                .await?
                != MessageResult::Yes
        {
            return Ok(());
        }
        let change = match request {
            Request::Edit(_) => {
                let current = selected.map(|entry| &entry.rule);
                let targets = catalog
                    .chains
                    .iter()
                    .filter(|name| {
                        selected.is_none_or(|entry| entry.key.chain.as_ref() != Some(*name))
                    })
                    .collect::<Vec<_>>();
                let sets = catalog.sets.iter().collect::<Vec<_>>();
                let Some(rule) = edit_rule(
                    &self.dialogs,
                    window,
                    &catalog.interfaces,
                    &targets,
                    &sets,
                    current,
                    Some((self.scope.family, self.scope.kind)),
                )
                .await?
                else {
                    return Ok(());
                };
                if current == Some(&rule) {
                    return Ok(());
                }
                if let Some(selected) = selected {
                    Change::Put(selected.key.clone(), rule)
                } else {
                    Change::Add(rule)
                }
            }
            Request::Enable(index, enabled) => {
                Change::Enable(catalog.rows[index].key.clone(), enabled)
            }
            Request::Remove(index) => Change::Remove(catalog.rows[index].key.clone()),
        };
        let revision = catalog.revision;
        let document = self
            .background
            .run(move || catalog::candidate(domain.as_deref(), revision, change))?
            .await?;
        let outcome = apply(&self.background, document, revision).await?;
        if !outcome.saved {
            self.report(window, &outcome.message).await?;
        }
        Ok(())
    }

    async fn report(&self, window: &Window, message: &str) -> liteos_ui::Result<()> {
        self.dialogs
            .message(
                window,
                "防火墙规则",
                message,
                MessageButtons::Ok,
                MessageIcon::Error,
            )?
            .await?;
        Ok(())
    }
}

fn row_text(entry: &Entry, counts: Option<(u64, u64)>) -> String {
    let PreroutingRule {
        source,
        destination,
        prefix_length,
        destination_set,
        action,
        ingress,
        transport,
        disabled,
        ..
    } = &entry.rule;
    let source = source.map_or_else(String::new, |prefix| {
        format!("{}/{}", prefix.network, prefix.length)
    });
    let destination = destination.zip(*prefix_length).map_or_else(
        || {
            destination_set
                .as_ref()
                .map_or_else(String::new, |name| format!("@{}", name.as_str()))
        },
        |(network, len)| format!("{network}/{len}"),
    );
    let protocol = transport.map_or("", |service| match service.protocol {
        network_config::configuration::TransportProtocol::Tcp => "tcp",
        network_config::configuration::TransportProtocol::Udp => "udp",
    });
    let port = transport
        .and_then(|service| service.destination_port)
        .map_or_else(String::new, |port| port.to_string());
    let (bytes, packets) = counts.map_or_else(
        || (String::new(), String::new()),
        |(bytes, packets)| (bytes.to_string(), packets.to_string()),
    );
    format!(
        "{}\t{}\t{}\t{}\t{source}\t{destination}\t{protocol}\t{port}\t{}\t{bytes}\t{packets}",
        entry.key.index + 1,
        if *disabled { "X" } else { "" },
        configured_action(action),
        entry
            .key
            .chain
            .as_ref()
            .map_or("prerouting", |name| name.as_str()),
        ingress.as_ref().map_or("", |name| name.as_str()),
    )
}
