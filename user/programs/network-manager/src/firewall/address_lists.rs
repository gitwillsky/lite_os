//! 双栈防火墙的静态地址列表。列表、编辑与转发规则引用同一份配置。

mod catalog;
mod filter;
mod property;

use self::{
    catalog::{Catalog, Change, Entry, Member},
    filter::{Condition, Field},
    property::Property,
};
use crate::{
    child,
    configuration::apply,
    item_list::{
        Find, FindAction, Glyph, Item, Summary, Tool, filter as list_filter, spacer, toolbar,
    },
    item_properties::Request,
    workspace::Workspace,
};
use liteos_ui::{
    Application, Background, Column, Event, EventKind, Layout, ListOptions, ListStyle,
    MessageButtons, MessageIcon, Task, TreeRow, Widget, WidgetKind, Window, state,
};
use network_config::{
    Revision,
    configuration::{ConfigurationName, connections::Family},
};
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

const ALL: u64 = 1;

pub(super) struct Page {
    family: Family,
    background: Background,
    view: Rc<RefCell<View>>,
    filter: list_filter::Panel<Field>,
    observation: Option<Task<()>>,
    mutation: Option<Task<()>>,
}

struct View {
    scope: Option<Arc<str>>,
    visible: bool,
    busy: bool,
    catalog: Option<Catalog>,
    property: Option<Property>,
    conditions: Vec<Condition>,
    list: Widget,
    group: Widget,
    // 下拉选项的行号随代际变化。用配置名保留用户的筛选范围。
    selected_group: Option<ConfigurationName>,
    find: Find,
    add: Tool,
    remove: Tool,
    summary: Summary,
    _layout: [Widget; 2],
}

enum Target {
    List,
    Close(u64),
    Keep(u64, Member),
}

struct Mutation {
    revision: Revision,
    change: Change,
    target: Target,
}

impl Page {
    pub(super) fn new(
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
        let filter = list_filter::Panel::new(parent, &bar, &[Field::Name, Field::Address])?;
        let group = child(
            &bar,
            WidgetKind::Combo,
            "",
            Layout {
                width: 130,
                ..Layout::default()
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
        list.set_columns(&[
            Column {
                id: 1,
                title: "列表",
                width: 185,
            },
            Column {
                id: 2,
                title: "地址",
                width: 250,
            },
        ])?;
        let view = View {
            scope: None,
            visible: false,
            busy: false,
            catalog: None,
            property: None,
            conditions: Vec::new(),
            list,
            group,
            selected_group: None,
            find,
            add,
            remove,
            summary: Summary::new(parent)?,
            _layout: [bar, space],
        };
        view.buttons()?;
        Ok(Self {
            family,
            background,
            view: Rc::new(RefCell::new(view)),
            filter,
            observation: None,
            mutation: None,
        })
    }

    pub(super) fn show(
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
        if !visible {
            view.property = None;
            self.observation = None;
            return Ok(());
        }
        drop(view);
        self.observe(app, window)
    }

    pub(super) fn select_domain(
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
        view.catalog = None;
        view.selected_group = None;
        view.set_groups()?;
        view.render()?;
        drop(view);
        self.observe(app, window)
    }

    fn observe(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        self.observation = None;
        let view = self.view.clone();
        let state = view.borrow();
        if !state.visible {
            return Ok(());
        }
        let scope = state.scope.clone();
        if state.catalog.is_none() {
            state.summary.notice("正在读取地址列表…")?;
        }
        drop(state);
        let family = self.family;
        let background = self.background.clone();
        let clock = app.clock();
        self.observation = Some(app.spawn_local(window, async move {
            loop {
                let revision = view
                    .borrow()
                    .catalog
                    .as_ref()
                    .map(|catalog| catalog.revision);
                let domain = scope.clone();
                let result = match background
                    .run(move || Catalog::load(domain.as_deref(), family, revision))
                {
                    Ok(task) => task.await.map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                let shown = match result {
                    Ok(Some(catalog)) => view.borrow_mut().present(catalog),
                    Ok(None) => Ok(()),
                    Err(error) => {
                        let mut view = view.borrow_mut();
                        view.catalog = None;
                        view.render().and_then(|()| {
                            view.summary.notice(&format!("无法读取地址列表。{error}"))
                        })
                    }
                };
                if shown.is_err() {
                    break;
                }
                let Ok(sleep) = clock.sleep(Duration::from_secs(2)) else {
                    break;
                };
                sleep.await;
            }
        })?);
        Ok(())
    }

    pub(super) fn handle(
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
            list_filter::Action::Apply(conditions) => {
                view.conditions = conditions;
                return view.render();
            }
            list_filter::Action::Invalid(message) => return view.summary.notice(&message),
            list_filter::Action::None => {}
        }
        match view.find.handle(event)? {
            Some(FindAction::Find(scan)) => {
                return view.find.scan_rows(&view.list, view.rows(), scan);
            }
            Some(FindAction::Close) => return view.find.present(&view.list),
            None => {}
        }
        if matches!(event.kind, EventKind::Select { .. }) {
            if event.node == view.group.id() {
                view.selected_group = view.group_choice()?.cloned();
                return view.render();
            }
            if event.node == view.list.id() {
                view.buttons()?;
                return view.update_summary();
            }
        }
        if view.busy {
            return Ok(());
        }
        let mutation =
            if let Some(request) = view.property.as_ref().and_then(|form| form.request(event)) {
                view.edit(self.family, request)?
            } else if matches!(event.kind, EventKind::Activate { .. }) {
                view.activate(workspace, event)?
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
        let background = self.background.clone();
        let parent = window.clone();
        let dialogs = app.dialogs();
        let task = app.spawn_local(window, async move {
            let domain = scope.clone();
            let Mutation {
                revision,
                change,
                target,
            } = mutation;
            let result = async {
                let document = background
                    .run(move || catalog::candidate(domain.as_deref(), revision, change))?
                    .await?;
                apply(&background, document, revision).await
            }
            .await;
            let (saved, committed, notice) = match result {
                Ok(outcome) => (outcome.saved, outcome.saved_revision, outcome.message),
                Err(error) => (false, None, error.to_string()),
            };
            let report = {
                let mut view = view.borrow_mut();
                view.busy = false;
                let same_view = view.visible && view.scope == scope;
                if saved
                    && same_view
                    && view
                        .catalog
                        .as_ref()
                        .is_some_and(|catalog| catalog.revision == revision)
                {
                    // 禁止继续在旧代际操作；等观察任务发布新列表。
                    view.catalog = None;
                    let _ = view.render();
                }
                let _ = view.buttons();
                if !same_view {
                    return;
                }
                match target {
                    Target::List => !saved,
                    Target::Close(frame) | Target::Keep(frame, _) => {
                        if let Some(form) = view
                            .property
                            .as_mut()
                            .filter(|form| form.frame.id() == frame)
                        {
                            match (saved, committed, target) {
                                (true, _, Target::Close(_)) => view.property = None,
                                (true, Some(revision), Target::Keep(_, member)) => {
                                    let _ = form.bind(revision, member);
                                    let _ = form.note("");
                                }
                                (true, None, _) => {
                                    let _ = form.ready(true, false);
                                    let _ = form.note("已保存。请关闭后从最新列表重新打开。");
                                }
                                _ => {
                                    let _ = form.ready(true, true);
                                    let _ = form.note(&notice);
                                }
                            }
                        }
                        false
                    }
                }
            };
            if report
                && let Ok(message) = dialogs.message(
                    &parent,
                    "防火墙地址列表",
                    &notice,
                    MessageButtons::Ok,
                    MessageIcon::Error,
                )
            {
                let _ = message.await;
            }
        })?;
        let mut view = self.view.borrow_mut();
        view.busy = true;
        self.mutation = Some(task);
        view.buttons()
    }
}

impl View {
    fn group_choice(&self) -> liteos_ui::Result<Option<&ConfigurationName>> {
        let index = self
            .group
            .selection()?
            .and_then(|id| id.checked_sub(ALL + 1));
        Ok(self
            .catalog
            .as_ref()
            .and_then(|catalog| catalog.names.get(index? as usize)))
    }

    fn set_groups(&mut self) -> liteos_ui::Result<()> {
        let names = self
            .catalog
            .as_ref()
            .map_or(&[][..], |catalog| catalog.names.as_slice());
        let mut selected = ALL;
        let rows: Vec<_> = std::iter::once("全部")
            .chain(names.iter().map(ConfigurationName::as_str))
            .enumerate()
            .map(|(i, text)| {
                let id = i as u64 + 1;
                if i > 0
                    && self
                        .selected_group
                        .as_ref()
                        .is_some_and(|name| name.as_str() == text)
                {
                    selected = id;
                }
                TreeRow {
                    id,
                    parent: 0,
                    text,
                    expanded: false,
                    has_children: false,
                }
            })
            .collect();
        self.group.set_tree_rows(&rows)?;
        if selected == ALL {
            self.selected_group = None;
        }
        self.group.select(Some(selected))
    }

    fn members(&self) -> impl Iterator<Item = (u64, &Entry)> {
        self.catalog
            .iter()
            .flat_map(|catalog| &catalog.entries)
            .enumerate()
            .filter(move |(_, entry)| {
                self.selected_group
                    .as_ref()
                    .is_none_or(|group| group == &entry.member.name)
                    && self
                        .conditions
                        .iter()
                        .all(|condition| condition.matches(&entry.member))
            })
            .map(|(index, entry)| (index as u64 + 1, entry))
    }

    fn rows(&self) -> impl Iterator<Item = (u64, &str)> {
        self.members().map(|(id, entry)| (id, entry.text.as_str()))
    }

    fn selected(&self) -> liteos_ui::Result<Option<&Member>> {
        let Some(selected) = self.list.selection()? else {
            return Ok(None);
        };
        Ok(self
            .members()
            .find(|(id, _)| *id == selected)
            .map(|(_, entry)| &entry.member))
    }

    fn buttons(&self) -> liteos_ui::Result<()> {
        let ready = self.catalog.is_some();
        self.add.set_enabled(ready && !self.busy)?;
        self.remove
            .set_enabled(!self.busy && self.selected()?.is_some())?;
        self.group
            .set_state(state::DROPDOWN_LIST | if ready { 0 } else { state::DISABLED })?;
        self.find.set_enabled(ready)
    }

    fn update_summary(&self) -> liteos_ui::Result<()> {
        self.summary.show(
            self.rows().count(),
            usize::from(self.selected()?.is_some()),
            "",
        )
    }

    fn present(&mut self, catalog: Catalog) -> liteos_ui::Result<()> {
        self.catalog = Some(catalog);
        self.list.select(None)?;
        self.set_groups()?;
        self.render()
    }

    fn render(&self) -> liteos_ui::Result<()> {
        let rows: Vec<_> = self.rows().collect();
        let selected = self
            .list
            .selection()?
            .filter(|id| rows.iter().any(|row| row.0 == *id));
        self.list.set_rows(&rows)?;
        self.list.select(selected)?;
        self.find
            .scan_rows(&self.list, rows.into_iter(), self.find.restart()?)?;
        self.buttons()?;
        self.update_summary()
    }

    fn activate(
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
        if let Some(form) = &self.property {
            workspace.raise(&form.frame)?;
            return Ok(None);
        }
        let revision = catalog.revision;
        let selected = self.selected()?.cloned();
        if remove {
            return Ok(selected.map(|member| Mutation {
                revision,
                change: Change::Remove(member),
                target: Target::List,
            }));
        }
        if add || selected.is_some() {
            self.property = Some(Property::new(
                workspace,
                revision,
                if add { None } else { selected },
                self.selected_group.as_ref(),
            )?);
        }
        Ok(None)
    }

    fn edit(&mut self, family: Family, request: Request) -> liteos_ui::Result<Option<Mutation>> {
        let Some(form) = self.property.as_ref() else {
            return Ok(None);
        };
        let frame = form.frame.id();
        let (change, target) = match request {
            Request::Close => {
                self.property = None;
                return Ok(None);
            }
            Request::Remove => {
                let Some(original) = &form.original else {
                    return Ok(None);
                };
                (Change::Remove(original.clone()), Target::Close(frame))
            }
            Request::Save { close } => {
                let Some(member) = form.value(family)? else {
                    return Ok(None);
                };
                if form.original.as_ref() == Some(&member) {
                    if close {
                        self.property = None;
                    } else {
                        form.note("")?;
                    }
                    return Ok(None);
                }
                let target = if close {
                    Target::Close(frame)
                } else {
                    Target::Keep(frame, member.clone())
                };
                (
                    Change::Put {
                        original: form.original.clone(),
                        member,
                    },
                    target,
                )
            }
        };
        form.ready(false, false)?;
        form.note("正在保存…")?;
        Ok(Some(Mutation {
            revision: form.revision,
            change,
            target,
        }))
    }
}
