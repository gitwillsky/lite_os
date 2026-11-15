//! IPv4 与 IPv6 地址窗口共用列表、查找、筛选和属性生命周期。

mod catalog;
mod configured;
mod property;

pub(super) use network_config::configuration::RoutingFamily as Family;

use self::{
    catalog::{Catalog, Change, Entry, Member},
    property::{Property, lifetime},
};
use crate::{
    child,
    configuration::{DraftBaseline, apply},
    item_list::{Find, FindAction, Glyph, Item, Summary, Tool, filter, spacer, toolbar},
    item_properties::Request,
    workspace::Workspace,
};
use liteos_ui::{
    Application, Background, Column, Event, EventKind, Layout, ListOptions, ListStyle,
    MessageButtons, MessageIcon, Task, Widget, WidgetKind, Window,
};
use network_config::{
    Revision,
    configuration::{IpPrefix, RoutePolicyPrefix},
};
use std::{cell::RefCell, io, net::IpAddr, rc::Rc, sync::Arc, time::Duration};

pub(crate) struct Page {
    view: Rc<RefCell<View>>,
    background: Background,
    filter: filter::Panel<Field>,
    observer: Option<Task<()>>,
    writer: Option<Task<()>>,
}

struct View {
    family: Family,
    visible: bool,
    busy: bool,
    scope: Option<Arc<str>>,
    catalog: Option<Catalog>,
    property: Option<Property>,
    conditions: Vec<Condition>,
    list: Widget,
    find: Find,
    add: Tool,
    remove: Tool,
    summary: Summary,
    _decor: [Widget; 2],
}

struct Mutation {
    baseline: DraftBaseline,
    change: Change,
    target: AfterSave,
}

enum AfterSave {
    List,
    Close(u64),
    Keep(u64, Member),
}

impl Page {
    pub(crate) fn new(
        parent: &Widget,
        background: Background,
        family: Family,
    ) -> liteos_ui::Result<Self> {
        let bar = child(parent, WidgetKind::Row, "", Layout::default())?;
        let [add, remove] = toolbar(
            &bar,
            [
                Item::Command(Glyph::Add, "新建"),
                Item::Command(Glyph::Remove, "删除"),
            ],
        )?;
        let space = spacer(&bar)?;
        let find = Find::new(&bar)?;
        let filter = filter::Panel::new(
            parent,
            &bar,
            match family {
                Family::Ipv4 => &[
                    Field::Address(Family::Ipv4),
                    Field::Network,
                    Field::Interface,
                ],
                Family::Ipv6 => &[Field::Address(Family::Ipv6), Field::Interface],
            },
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
        let fields: &[(&str, i32)] = match family {
            Family::Ipv4 => &[("", 40), ("地址", 182), ("网络", 150), ("接口", 134)],
            Family::Ipv6 => &[
                ("", 40),
                ("地址", 255),
                ("接口", 134),
                ("有效寿命", 112),
                ("首选寿命", 112),
            ],
        };
        let columns: Vec<_> = fields
            .iter()
            .enumerate()
            .map(|(index, &(title, width))| Column {
                id: index as u64 + 1,
                title,
                width,
            })
            .collect();
        list.set_columns(&columns)?;
        let view = View {
            family,
            visible: false,
            busy: false,
            scope: None,
            catalog: None,
            property: None,
            conditions: Vec::new(),
            list,
            find,
            add,
            remove,
            summary: Summary::new(parent)?,
            _decor: [bar, space],
        };
        view.state()?;
        Ok(Self {
            view: Rc::new(RefCell::new(view)),
            background,
            filter,
            observer: None,
            writer: None,
        })
    }

    pub(crate) fn show(
        &mut self,
        app: &mut Application,
        window: &Window,
        visible: bool,
    ) -> liteos_ui::Result<()> {
        let mut view = self.view.borrow_mut();
        if view.visible == visible {
            return Ok(());
        }
        view.visible = visible;
        view.property = None;
        drop(view);
        self.observe(app, window)
    }

    pub(crate) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        scope: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        let mut view = self.view.borrow_mut();
        if view.scope == scope {
            return Ok(());
        }
        view.scope = scope;
        view.property = None;
        drop(view);
        self.observe(app, window)
    }

    fn observe(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        self.observer = None;
        let mut view = self.view.borrow_mut();
        view.catalog = None;
        view.render(None, true)?;
        if !view.visible {
            return Ok(());
        }
        view.summary.notice("正在读取地址…")?;
        let scope = view.scope.clone();
        let family = view.family;
        drop(view);
        let view = self.view.clone();
        let background = self.background.clone();
        let clock = app.clock();
        self.observer = Some(app.spawn_local(window, async move {
            loop {
                let domain = scope.clone();
                let result = match background.run(move || catalog::load(domain.as_deref(), family))
                {
                    Ok(task) => task.await,
                    Err(error) => Err(io::Error::other(error.to_string())),
                };
                let shown = match result {
                    Ok(catalog) => view.borrow_mut().present(catalog),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(()),
                    Err(error) => {
                        let mut view = view.borrow_mut();
                        view.catalog = None;
                        view.render(None, true).and_then(|()| {
                            view.summary
                                .notice(&format!("无法读取{}。{error}", title(family)))
                        })
                    }
                };
                if shown.is_err() {
                    break;
                }
                let Ok(wait) = clock.sleep(Duration::from_secs(2)) else {
                    break;
                };
                wait.await;
            }
        })?);
        Ok(())
    }

    pub(crate) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        workspace: &Workspace,
        event: &Event,
    ) -> liteos_ui::Result<()> {
        let mut view = self.view.borrow_mut();
        if !view.visible {
            return Ok(());
        }
        match self.filter.handle(event, &view.conditions)? {
            filter::Action::Apply(conditions) => {
                let previous = view.selected()?.cloned();
                view.conditions = conditions;
                return view.render(previous.as_ref(), true);
            }
            filter::Action::Invalid(message) => return view.summary.notice(&message),
            filter::Action::None => {}
        }
        match view.find.handle(event)? {
            Some(FindAction::Find(scan)) => {
                let text = view.rows();
                return view.find.scan_rows(
                    &view.list,
                    text.iter().map(|(id, text)| (*id, text.as_str())),
                    scan,
                );
            }
            Some(FindAction::Close) => return view.find.present(&view.list),
            None => {}
        }
        if event.node == view.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            return view.state();
        }
        if view.busy {
            return Ok(());
        }
        if let Some(property) = &view.property {
            property.address_changed(event)?;
        }
        let mutation =
            if let Some(request) = view.property.as_ref().and_then(|form| form.request(event)) {
                view.property_action(request)?
            } else if matches!(event.kind, EventKind::Activate { .. }) {
                view.list_action(workspace, event)?
            } else {
                None
            };
        drop(view);
        if let Some(mutation) = mutation {
            self.submit(app, window, mutation)?;
        }
        Ok(())
    }

    fn submit(
        &mut self,
        app: &mut Application,
        window: &Window,
        mutation: Mutation,
    ) -> liteos_ui::Result<()> {
        let view = self.view.clone();
        let scope = view.borrow().scope.clone();
        let family = view.borrow().family;
        let background = self.background.clone();
        let dialogs = app.dialogs();
        let parent = window.clone();
        let job = app.spawn_local(window, async move {
            let Mutation {
                baseline,
                change,
                target,
            } = mutation;
            let domain = scope.clone();
            let result = async {
                let (revision, candidate) = background
                    .run(move || configured::candidate(domain.as_deref(), baseline, change))?
                    .await?;
                let result = apply(&background, candidate.clone(), revision).await?;
                Ok::<_, Box<dyn std::error::Error>>((revision, result, candidate))
            }
            .await;
            let (previous, saved, committed, message) = match result {
                Ok((revision, result, candidate)) => (
                    Some(revision),
                    result.saved,
                    result
                        .saved_revision
                        .map(|saved| DraftBaseline::new(saved, candidate)),
                    result.message,
                ),
                Err(error) => (None, false, None, error.to_string()),
            };
            let alert = {
                let mut view = view.borrow_mut();
                view.busy = false;
                let _ = view.state();
                if !view.visible || view.scope != scope {
                    return;
                }
                if saved
                    && view
                        .catalog
                        .as_ref()
                        .is_some_and(|catalog| revision_of(catalog) == previous)
                {
                    // 已保存的代际由观察任务重新发布；此间禁用旧记录的编辑。
                    view.catalog = None;
                    let _ = view.render(None, true);
                }
                view.finish(target, saved, committed, &message)
                    .unwrap_or(false)
            };
            if alert
                && let Ok(dialog) = dialogs.message(
                    &parent,
                    title(family),
                    &message,
                    MessageButtons::Ok,
                    MessageIcon::Error,
                )
            {
                let _ = dialog.await;
            }
        })?;
        self.writer = Some(job);
        let mut view = self.view.borrow_mut();
        view.busy = true;
        view.state()
    }
}

impl View {
    fn entries(&self) -> impl Iterator<Item = (u64, &Entry)> {
        self.catalog
            .iter()
            .flat_map(|catalog| &catalog.entries)
            .enumerate()
            .filter(|(_, entry)| self.visible_entry(entry))
            .map(|(index, entry)| (index as u64 + 1, entry))
    }

    fn visible_entry(&self, entry: &Entry) -> bool {
        self.conditions
            .iter()
            .all(|condition| condition.matches(entry))
    }

    fn rows(&self) -> Vec<(u64, String)> {
        self.entries()
            .map(|(id, entry)| {
                let text = match self.family {
                    Family::Ipv4 => format!(
                        "{}\t{}\t{}\t{}",
                        entry.flags(),
                        entry.address_text(),
                        entry.network().expect("IPv4 window has only IPv4 entries"),
                        entry.interface
                    ),
                    Family::Ipv6 => {
                        let (preferred, valid) = entry
                            .runtime
                            .and_then(|runtime| runtime.lifetimes)
                            .map_or_else(
                                || (String::new(), String::new()),
                                |(preferred, valid)| (lifetime(preferred), lifetime(valid)),
                            );
                        format!(
                            "{}\t{}\t{}\t{valid}\t{preferred}",
                            entry.flags(),
                            entry.address_text(),
                            entry.interface
                        )
                    }
                };
                (id, text)
            })
            .collect()
    }

    fn selected(&self) -> liteos_ui::Result<Option<&Entry>> {
        let id = self.list.selection()?.and_then(|id| id.checked_sub(1));
        Ok(self
            .catalog
            .as_ref()
            .and_then(|catalog| catalog.entries.get(id? as usize))
            .filter(|entry| self.visible_entry(entry)))
    }

    fn state(&self) -> liteos_ui::Result<()> {
        let selected = self.selected()?;
        self.add.set_enabled(
            !self.busy
                && self
                    .catalog
                    .as_ref()
                    .is_some_and(|catalog| catalog.targets.iter().any(|target| target.available)),
        )?;
        self.remove
            .set_enabled(!self.busy && selected.is_some_and(|entry| entry.member.is_some()))?;
        self.find.set_enabled(self.catalog.is_some())?;
        self.summary
            .show(self.entries().count(), usize::from(selected.is_some()), "")
    }

    fn render(&self, selection: Option<&Entry>, restart: bool) -> liteos_ui::Result<()> {
        let text = self.rows();
        let selection = selection.and_then(|previous| {
            self.entries()
                .find_map(|(id, entry)| previous.same_address(entry).then_some(id))
        });
        let rows: Vec<_> = text.iter().map(|(id, text)| (*id, text.as_str())).collect();
        self.list.set_rows(&rows)?;
        self.list.select(selection)?;
        if restart {
            self.find
                .scan_rows(&self.list, rows.into_iter(), self.find.restart()?)?;
        } else {
            self.find.present(&self.list)?;
        }
        self.state()
    }

    fn present(&mut self, catalog: Catalog) -> liteos_ui::Result<()> {
        let previous = self.selected()?.cloned();
        let old = self.catalog.replace(catalog);
        let Some(current) = &self.catalog else {
            unreachable!("catalog was replaced")
        };
        let restart = old.as_ref().is_none_or(|old| {
            old.entries.len() != current.entries.len()
                || old
                    .entries
                    .iter()
                    .zip(&current.entries)
                    .any(|(old, current)| !old.same_address(current))
        });
        if restart
            || old.as_ref().is_some_and(|old| {
                revision_of(old) != revision_of(current)
                    || old.entries != current.entries
                    || old.targets != current.targets
            })
        {
            self.render(previous.as_ref(), restart)?;
        }
        Ok(())
    }

    fn list_action(
        &mut self,
        workspace: &Workspace,
        event: &Event,
    ) -> liteos_ui::Result<Option<Mutation>> {
        let Some(catalog) = &self.catalog else {
            return Ok(None);
        };
        let add = self.add.activated(event);
        let remove = self.remove.activated(event);
        if !add && !remove && event.node != self.list.id() {
            return Ok(None);
        }
        if let Some(property) = &self.property {
            workspace.raise(&property.frame)?;
            return Ok(None);
        }
        let entry = self.selected()?;
        if remove {
            return Ok(catalog
                .baseline
                .clone()
                .zip(entry.and_then(|entry| entry.member.clone()))
                .map(|(baseline, member)| Mutation {
                    baseline,
                    change: Change::Remove(member),
                    target: AfterSave::List,
                }));
        }
        if add || entry.is_some() {
            self.property = Some(Property::new(
                workspace,
                catalog,
                self.family,
                if add { None } else { entry },
            )?);
        }
        Ok(None)
    }

    fn property_action(&mut self, request: Request) -> liteos_ui::Result<Option<Mutation>> {
        let Some(property) = &self.property else {
            return Ok(None);
        };
        let frame = property.frame.id();
        let (change, target) = match request {
            Request::Close => {
                self.property = None;
                return Ok(None);
            }
            Request::Remove => {
                let Some(original) = &property.original else {
                    return Ok(None);
                };
                (Change::Remove(original.clone()), AfterSave::Close(frame))
            }
            Request::Save { close } => {
                let Some(next) = property.value()? else {
                    return Ok(None);
                };
                if property.original.as_ref() == Some(&next) {
                    if close {
                        self.property = None;
                    } else {
                        property.note("")?;
                    }
                    return Ok(None);
                }
                let target = if close {
                    AfterSave::Close(frame)
                } else {
                    AfterSave::Keep(frame, next.clone())
                };
                (
                    Change::Put {
                        previous: property.original.clone(),
                        next,
                    },
                    target,
                )
            }
        };
        let Some(baseline) = property.baseline.clone() else {
            return Ok(None);
        };
        property.ready(false, false)?;
        property.note("正在保存…")?;
        Ok(Some(Mutation {
            baseline,
            change,
            target,
        }))
    }

    fn finish(
        &mut self,
        target: AfterSave,
        saved: bool,
        baseline: Option<DraftBaseline>,
        message: &str,
    ) -> liteos_ui::Result<bool> {
        let frame = match target {
            AfterSave::List => return Ok(!saved),
            AfterSave::Close(frame) | AfterSave::Keep(frame, _) => frame,
        };
        let Some(property) = self
            .property
            .as_mut()
            .filter(|property| property.frame.id() == frame)
        else {
            return Ok(false);
        };
        match (saved, baseline, target) {
            (true, _, AfterSave::Close(_)) => self.property = None,
            (true, Some(baseline), AfterSave::Keep(_, member)) => {
                property.bind(baseline, member)?;
                property.note("")?;
            }
            (true, None, _) => {
                property.ready(true, false)?;
                property.note("已保存。请关闭后从最新地址列表重新打开。")?;
            }
            _ => {
                property.ready(true, true)?;
                property.note(message)?;
            }
        }
        Ok(false)
    }
}

fn revision_of(catalog: &Catalog) -> Option<Revision> {
    catalog.baseline.as_ref().map(|baseline| baseline.revision)
}

fn title(family: Family) -> &'static str {
    match family {
        Family::Ipv4 => "地址",
        Family::Ipv6 => "IPv6 地址",
    }
}

fn is_family(family: Family, address: IpAddr) -> bool {
    address.is_ipv4() == (family == Family::Ipv4)
}

#[derive(Clone, Copy)]
enum Field {
    Address(Family),
    Network,
    Interface,
}

#[derive(PartialEq)]
struct Condition {
    operand: Operand,
    exclude: bool,
}

#[derive(PartialEq)]
enum Operand {
    Address(IpPrefix),
    Network(IpPrefix),
    Interface(String),
}

impl filter::Field for Field {
    type Condition = Condition;

    fn label(self) -> &'static str {
        match self {
            Self::Address(_) => "地址",
            Self::Network => "网络",
            Self::Interface => "接口",
        }
    }

    fn modes(self) -> (&'static str, &'static str) {
        match self {
            Self::Address(_) | Self::Network => ("属于", "不属于"),
            Self::Interface => ("=", "≠"),
        }
    }

    fn parse(self, exclude: bool, text: &str) -> Result<Condition, &'static str> {
        let operand = match self {
            Self::Address(family) => Operand::Address(parse_filter(text, family)?),
            Self::Network => Operand::Network(parse_filter(text, Family::Ipv4)?),
            Self::Interface => Operand::Interface(text.to_owned()),
        };
        Ok(Condition { operand, exclude })
    }
}

fn parse_filter(text: &str, family: Family) -> Result<IpPrefix, &'static str> {
    let text = text.trim();
    text.parse::<IpAddr>()
        .map(|network| IpPrefix {
            network,
            length: if network.is_ipv4() { 32 } else { 128 },
        })
        .ok()
        .or_else(|| text.parse::<IpPrefix>().ok())
        .filter(|prefix| is_family(family, prefix.network))
        .ok_or("请输入当前地址族的 IP 地址或规范 CIDR 网段")
}

fn contains(prefix: IpPrefix, address: IpAddr) -> bool {
    RoutePolicyPrefix {
        network: prefix.network,
        length: prefix.length,
        minimum: Some(prefix.length),
        maximum: None,
    }
    .matches(address, if address.is_ipv4() { 32 } else { 128 })
}

impl Condition {
    fn matches(&self, entry: &Entry) -> bool {
        let matched = match &self.operand {
            Operand::Address(prefix) => contains(*prefix, entry.address),
            Operand::Network(prefix) => entry
                .network()
                .is_some_and(|address| contains(*prefix, address.into())),
            Operand::Interface(interface) => &entry.interface == interface,
        };
        matched != self.exclude
    }
}
