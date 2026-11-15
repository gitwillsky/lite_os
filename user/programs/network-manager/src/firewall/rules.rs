//! 过滤和 NAT 共用列表、只读计数采样与按代际提交。

use super::{
    catalog::{self, Catalog, Change, Counts, Entry, Key, Scope},
    property::Property,
};
use crate::{
    child,
    configuration::apply,
    item_list::{Glyph, Item, Summary, Tool, toolbar},
    item_properties::Request,
    routes::rule_address,
    workspace::Workspace,
};
use liteos_ui::{
    Application, Background, Clock, Column, Dialogs, Event, EventKind, Layout, ListOptions,
    ListStyle, MessageButtons, MessageIcon, MessageResult, Task, Widget, WidgetKind, Window, state,
};
use network_config::{Revision, configuration::PreroutingRule, firewall::configured_action};
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

impl Sample {
    fn counts_for(&self, property: &Property) -> Option<(u64, u64)> {
        let (key, _) = property.original.as_ref()?;
        if property.revision != self.catalog.revision {
            return None;
        }
        self.catalog.rows[self.start..]
            .iter()
            .zip(&self.counts)
            .find_map(|(entry, count)| (&entry.key == key).then_some(*count).flatten())
    }
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
    property: RefCell<Option<Property>>,
    // 对话框与提交共用操作期，防止异步采样重开按钮。
    editing: Cell<bool>,
    background: Background,
    clock: Clock,
    dialogs: Dialogs,
}

struct Mutation {
    revision: Revision,
    change: Change,
    shared: bool,
    finish: Finish,
}

struct Saved {
    revision: Option<Revision>,
    key: Option<Key>,
}

enum Finish {
    List,
    Close(u64),
    Keep { frame: u64, rule: PreroutingRule },
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
            property: RefCell::default(),
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
            self.view.property.borrow_mut().take();
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
        self.view.property.borrow_mut().take();
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
        workspace: &Workspace,
        event: &Event,
    ) -> liteos_ui::Result<()> {
        if !self.visible {
            return Ok(());
        }
        let view = self.view.clone();
        if !view.editing.get()
            && let Some(mutation) = view.property_command(event)?
        {
            return self.submit(app, window, mutation);
        }
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
        let catalog = view
            .sample
            .borrow()
            .as_ref()
            .expect("ready sample")
            .catalog
            .clone();
        let selected = view.selected()?.map(|index| &catalog.rows[index]);
        if view.new.activated(event) || event.node == view.list.id() && selected.is_some() {
            let entry = if view.new.activated(event) {
                None
            } else {
                selected
            };
            return view.open_property(workspace, &catalog, entry);
        }
        let Some(entry) = selected else {
            return Ok(());
        };
        let change = if view.enable.activated(event) {
            Change::Enable(entry.key.clone(), true)
        } else if view.disable.activated(event) {
            Change::Enable(entry.key.clone(), false)
        } else if view.remove.activated(event) {
            Change::Remove(entry.key.clone())
        } else {
            return Ok(());
        };
        self.submit(
            app,
            window,
            Mutation {
                revision: catalog.revision,
                change,
                shared: rule_address(&entry.rule).is_none(),
                finish: Finish::List,
            },
        )
    }

    fn submit(
        &mut self,
        app: &mut Application,
        window: &Window,
        mutation: Mutation,
    ) -> liteos_ui::Result<()> {
        let view = self.view.clone();
        view.editing.set(true);
        view.buttons()?;
        let scope = self.domain.clone();
        let parent = window.clone();
        self.mutation = Some(app.spawn_local(window, async move {
            let finish = mutation.finish;
            let result = view
                .mutate(
                    &parent,
                    scope,
                    mutation.revision,
                    mutation.change,
                    mutation.shared,
                )
                .await;
            if let Err(error) = &result
                && matches!(finish, Finish::List)
            {
                let _ = view.report(&parent, &error.to_string()).await;
            }
            view.editing.set(false);
            let _ = view.finish(finish, result);
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
        if let Some(property) = self.property.borrow().as_ref() {
            property.counts(sample.counts_for(property))?;
        }
        self.summary()?;
        self.buttons()
    }

    fn open_property(
        &self,
        workspace: &Workspace,
        catalog: &Catalog,
        entry: Option<&Entry>,
    ) -> liteos_ui::Result<()> {
        let mut property = self.property.borrow_mut();
        if let Some(current) = property.as_ref()
            && current.revision == catalog.revision
            && current.original.as_ref().map(|(key, _)| key) == entry.map(|entry| &entry.key)
        {
            return workspace.raise(&current.frame);
        }
        let form = Property::new(workspace, catalog, self.scope, entry)?;
        if let Some(sample) = self.sample.borrow().as_ref() {
            form.counts(sample.counts_for(&form))?;
        }
        *property = Some(form);
        Ok(())
    }

    fn property_command(&self, event: &Event) -> liteos_ui::Result<Option<Mutation>> {
        let mut property = self.property.borrow_mut();
        let Some(form) = property.as_ref() else {
            return Ok(None);
        };
        let Some(request) = form.request(event)? else {
            return Ok(None);
        };
        let frame = form.frame.id();
        let original = form.original.as_ref();
        let (change, finish) = match request {
            Request::Close => {
                property.take();
                return Ok(None);
            }
            Request::Remove => {
                let Some((key, _)) = original else {
                    return Ok(None);
                };
                (Change::Remove(key.clone()), Finish::Close(frame))
            }
            Request::Save { close } => {
                let Some(rule) = form.value()? else {
                    return Ok(None);
                };
                if original.is_some_and(|(_, old)| old == &rule) {
                    if close {
                        property.take();
                    } else {
                        form.note("")?;
                    }
                    return Ok(None);
                }
                let change = if let Some((key, _)) = original {
                    Change::Put(key.clone(), rule.clone())
                } else {
                    Change::Add(rule.clone())
                };
                let finish = if close {
                    Finish::Close(frame)
                } else {
                    Finish::Keep { frame, rule }
                };
                (change, finish)
            }
        };
        form.ready(false)?;
        form.note("")?;
        Ok(Some(Mutation {
            revision: form.revision,
            shared: original.is_some_and(|(_, rule)| rule_address(rule).is_none()),
            change,
            finish,
        }))
    }

    async fn mutate(
        &self,
        window: &Window,
        domain: Option<Arc<str>>,
        revision: Revision,
        change: Change,
        shared: bool,
    ) -> Result<Option<Saved>, Error> {
        if shared
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
            return Ok(None);
        }
        let (document, key) = self
            .background
            .run(move || catalog::candidate(domain.as_deref(), revision, change))?
            .await?;
        let outcome = apply(&self.background, document, revision).await?;
        if !outcome.saved {
            return Err(std::io::Error::other(outcome.message).into());
        }
        Ok(Some(Saved {
            revision: outcome.saved_revision,
            key,
        }))
    }

    fn finish(
        &self,
        finish: Finish,
        result: Result<Option<Saved>, Error>,
    ) -> liteos_ui::Result<()> {
        let frame = match finish {
            Finish::List => return Ok(()),
            Finish::Close(frame) | Finish::Keep { frame, .. } => frame,
        };
        let mut property = self.property.borrow_mut();
        let Some(form) = property.as_mut().filter(|form| form.frame.id() == frame) else {
            return Ok(());
        };
        match result {
            Ok(None) => form.ready(true),
            Err(error) => {
                form.ready(true)?;
                form.note(&error.to_string())
            }
            Ok(Some(saved)) => match finish {
                Finish::Close(_) => {
                    property.take();
                    Ok(())
                }
                Finish::Keep { rule, .. } => {
                    if let (Some(revision), Some(key)) = (saved.revision, saved.key) {
                        form.bind(revision, key, rule)?;
                        form.counts(None)
                    } else {
                        form.close_only()?;
                        form.note("已保存。请从最新列表重新打开。")
                    }
                }
                Finish::List => unreachable!("list has no property"),
            },
        }
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
