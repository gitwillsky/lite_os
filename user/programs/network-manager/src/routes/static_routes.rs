//! 「IP / IPv6 → 路由」的双栈列表；只有配置声明拥有可修改的身份。

mod catalog;
mod immediate;
mod installed;
mod property;

use self::{
    catalog::{Catalog, Change, Entry, table_name},
    installed::Installed,
    property::{Binding, Editor, ObservedProperty, Request},
};
use crate::{
    child,
    configuration::apply,
    item_list::{Find, FindAction, Glyph, Item, Summary, Tool, spacer, toolbar},
    workspace::Workspace,
};
use liteos_ui::{
    Application, Background, Clock, Column, Event, EventKind, Layout, ListOptions, ListStyle, Task,
    TreeRow, Widget, WidgetKind, Window, state,
};
use network_config::{
    Revision,
    configuration::StaticRoute,
    routes::{Family, TABLE_MAIN},
};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

const FAMILIES: [Family; 2] = [Family::Ipv4, Family::Ipv6];
const ALL_TABLES: u64 = u64::MAX;

struct List {
    _layout: [Widget; 2],
    widget: Widget,
    add: Tool,
    enable: Tool,
    disable: Tool,
    remove: Tool,
    find: Find,
    table: Widget,
    summary: Summary,
    // SDK 不暴露行数；保留筛选后的可见计数，选中变化时无需重建列表。
    displayed: Cell<usize>,
}

enum Selection<'a> {
    Declaration(&'a Entry),
    Observed(&'a Installed),
}

impl<'a> Selection<'a> {
    fn declaration(self) -> Option<&'a Entry> {
        match self {
            Self::Declaration(entry) => Some(entry),
            Self::Observed(_) => None,
        }
    }
}

impl List {
    fn new(parent: &Widget) -> liteos_ui::Result<Self> {
        let bar = child(parent, WidgetKind::Row, "", Layout::default())?;
        let [add, _, enable, disable, remove] = toolbar(
            &bar,
            [
                Item::Command(Glyph::Add, "新建"),
                Item::Separator,
                Item::Command(Glyph::Enable, "启用"),
                Item::Command(Glyph::Disable, "停用"),
                Item::Command(Glyph::Remove, "删除"),
            ],
        )?;
        let spacer = spacer(&bar)?;
        let find = Find::new(&bar)?;
        let table = child(
            &bar,
            WidgetKind::Combo,
            "",
            Layout {
                width: 84,
                ..Layout::default()
            },
        )?;
        table.set_state(state::DROPDOWN_LIST | state::DISABLED)?;
        let widget = child(
            parent,
            WidgetKind::List,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        widget.set_list_style(
            ListStyle::Details,
            ListOptions {
                multiple: false,
                full_row_select: true,
            },
        )?;
        let fields = [
            ("", 57),
            ("目的地址", 155),
            ("网关", 235),
            ("距离", 80),
            ("路由表", 103),
        ];
        widget.set_columns(&std::array::from_fn::<_, 5, _>(|i| Column {
            id: i as u64 + 1,
            title: fields[i].0,
            width: fields[i].1,
        }))?;
        let list = Self {
            _layout: [bar, spacer],
            widget,
            add,
            enable,
            disable,
            remove,
            find,
            table,
            summary: Summary::new(parent)?,
            displayed: Cell::new(0),
        };
        list.clear("正在读取路由…")?;
        Ok(list)
    }

    fn clear(&self, notice: &str) -> liteos_ui::Result<()> {
        self.add.set_enabled(false)?;
        self.table
            .set_state(state::DROPDOWN_LIST | state::DISABLED)?;
        self.widget.set_rows(&[])?;
        self.widget.select(None)?;
        self.find.locate(&self.widget, None)?;
        self.displayed.set(0);
        self.selection_changed(None, "")?;
        self.summary.notice(notice)
    }

    fn set_tables(&self, tables: &[u32]) -> liteos_ui::Result<()> {
        let selected = self.table.selection()?.unwrap_or(ALL_TABLES);
        let names: Vec<_> = tables.iter().copied().map(table_name).collect();
        let rows: Vec<_> = std::iter::once((ALL_TABLES, "全部"))
            .chain(
                tables
                    .iter()
                    .copied()
                    .map(u64::from)
                    .zip(names.iter().map(String::as_str)),
            )
            .map(|(id, text)| TreeRow {
                id,
                parent: 0,
                text,
                expanded: false,
                has_children: false,
            })
            .collect();
        self.table.set_tree_rows(&rows)?;
        self.table
            .select(Some(if rows.iter().any(|row| row.id == selected) {
                selected
            } else {
                ALL_TABLES
            }))?;
        self.table.set_state(state::DROPDOWN_LIST)
    }

    fn render(
        &self,
        catalog: Option<&Catalog>,
        live: &[Installed],
        family: Family,
        selection: Option<u64>,
        notice: &str,
    ) -> liteos_ui::Result<()> {
        let mut tables = catalog.map_or_else(|| vec![TABLE_MAIN], |catalog| catalog.tables.clone());
        tables.extend(live.iter().map(|entry| entry.route.table));
        tables.sort_unstable();
        tables.dedup();
        self.set_tables(&tables)?;
        let rows = self.rows(catalog, live, family)?;
        let values: Vec<_> = rows.iter().map(|(id, text)| (*id, text.as_ref())).collect();
        self.widget.set_rows(&values)?;
        let selection = selection.filter(|id| rows.iter().any(|row| row.0 == *id));
        if self.widget.selection()? != selection {
            self.widget.select(selection)?;
        }
        self.add.set_enabled(catalog.is_some())?;
        self.displayed.set(rows.len());
        self.find
            .scan_rows(&self.widget, values.into_iter(), self.find.restart()?)?;
        self.selection_changed(self.selected(catalog, live, family)?, notice)
    }

    fn rows<'a>(
        &self,
        catalog: Option<&'a Catalog>,
        live: &[Installed],
        family: Family,
    ) -> liteos_ui::Result<Vec<(u64, Cow<'a, str>)>> {
        let table = self.table.selection()?.unwrap_or(ALL_TABLES);
        let accepts = |id| table == ALL_TABLES || table == u64::from(id);
        let entries = catalog.map_or(&[][..], |catalog| catalog.entries(family));
        let declared = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| accepts(entry.key.table))
            .map(|(index, entry)| {
                (
                    (
                        entry.route.destination,
                        entry.route.prefix_length,
                        entry.key.table,
                    ),
                    (declaration_id(index), Cow::Borrowed(entry.text.as_str())),
                )
            });
        let observed = live
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                if !accepts(entry.route.table) {
                    return false;
                }
                // STATIC 的生效行没有配置身份。同表同前缀的启用声明保留可写行，
                // 无法归属的实际路径仍可在「路由 → 路由表」查看。
                if entry.route.protocol != 4 {
                    return true;
                }
                let prefix = (entry.route.prefix.network, entry.route.prefix.length);
                let start = entries.partition_point(|entry| {
                    (entry.route.destination, entry.route.prefix_length) < prefix
                });
                !entries[start..]
                    .iter()
                    .take_while(|entry| {
                        (entry.route.destination, entry.route.prefix_length) == prefix
                    })
                    .any(|candidate| {
                        candidate.key.table == entry.route.table && !candidate.route.disabled
                    })
            })
            .map(|(index, entry)| {
                (
                    (
                        entry.route.prefix.network,
                        entry.route.prefix.length,
                        entry.route.table,
                    ),
                    (observed_id(index), Cow::Owned(entry.text())),
                )
            });
        let mut rows: Vec<_> = declared.chain(observed).collect();
        rows.sort_by_key(|(key, _)| *key);
        Ok(rows.into_iter().map(|(_, row)| row).collect())
    }

    fn selected<'a>(
        &self,
        catalog: Option<&'a Catalog>,
        live: &'a [Installed],
        family: Family,
    ) -> liteos_ui::Result<Option<Selection<'a>>> {
        let Some(id) = self.widget.selection()? else {
            return Ok(None);
        };
        let index = (id / 2) as usize;
        Ok(if id % 2 == 0 {
            catalog
                .and_then(|catalog| {
                    index
                        .checked_sub(1)
                        .and_then(|index| catalog.entries(family).get(index))
                })
                .map(Selection::Declaration)
        } else {
            live.get(index).map(Selection::Observed)
        })
    }

    fn selection_changed(
        &self,
        entry: Option<Selection<'_>>,
        notice: &str,
    ) -> liteos_ui::Result<()> {
        let selected = entry.is_some();
        let entry = entry.and_then(Selection::declaration);
        self.enable
            .set_enabled(entry.is_some_and(|entry| entry.route.disabled))?;
        self.disable
            .set_enabled(entry.is_some_and(|entry| !entry.route.disabled))?;
        self.remove.set_enabled(entry.is_some())?;
        self.summary
            .show(self.displayed.get(), usize::from(selected), notice)
    }
}

// 两套身份空间只用于列表行号。声明只能以同一配置代际下的 Key 写回。
fn declaration_id(index: usize) -> u64 {
    (index as u64 + 1) * 2
}

fn observed_id(index: usize) -> u64 {
    index as u64 * 2 + 1
}

#[derive(Default)]
struct Live {
    rows: Vec<Installed>,
    error: Option<String>,
}

struct View {
    lists: [List; 2],
    catalog: Option<Catalog>,
    configuration_error: Option<String>,
    live: [Live; 2],
    editor: Option<(Family, Editor)>,
    observed_property: Option<(Family, ObservedProperty)>,
    scope: Option<Arc<str>>,
    visible: [bool; 2],
}

impl View {
    fn clear(&mut self, notice: &str) -> liteos_ui::Result<()> {
        self.catalog = None;
        self.configuration_error = None;
        self.live = Default::default();
        self.observed_property = None;
        for (list, visible) in self.lists.iter().zip(self.visible) {
            if visible {
                list.clear(notice)?;
            }
        }
        Ok(())
    }

    fn notice(&self, index: usize) -> &str {
        self.live[index]
            .error
            .as_deref()
            .or(self.configuration_error.as_deref())
            .unwrap_or("实时")
    }

    fn render(&self, index: usize, selection: Option<u64>) -> liteos_ui::Result<()> {
        self.lists[index].render(
            self.catalog.as_ref(),
            &self.live[index].rows,
            FAMILIES[index],
            selection,
            self.notice(index),
        )
    }

    fn render_declarations_changed(&self) -> liteos_ui::Result<()> {
        for (i, list) in self.lists.iter().enumerate() {
            if self.visible[i] {
                let selection = list.widget.selection()?.filter(|id| id % 2 == 1);
                self.render(i, selection)?;
            }
        }
        Ok(())
    }

    fn present_configuration(
        &mut self,
        result: Result<Option<Catalog>, String>,
    ) -> liteos_ui::Result<()> {
        let changed = match result {
            Ok(Some(catalog)) => {
                self.catalog = Some(catalog);
                self.configuration_error = None;
                true
            }
            Ok(None) => false,
            Err(error) => {
                let error = format!("无法读取声明：{error}");
                self.catalog = None;
                self.configuration_error.replace(error.clone()).as_ref() != Some(&error)
            }
        };
        if changed {
            self.render_declarations_changed()?;
            if let Some((_, editor)) = &self.editor {
                editor.present_status(None)?;
            }
        }
        Ok(())
    }

    fn present_selection(
        &mut self,
        revision: Revision,
        selection: Option<&catalog::Selections>,
    ) -> liteos_ui::Result<()> {
        let Some(catalog) = self
            .catalog
            .as_mut()
            .filter(|catalog| catalog.revision == revision)
        else {
            return Ok(());
        };
        if let Some((_, editor)) = &self.editor {
            editor.present_status(
                (editor.binding.revision == revision)
                    .then_some(selection)
                    .flatten(),
            )?;
        }
        if catalog.show_selection(selection) {
            for (i, list) in self.lists.iter().enumerate() {
                if self.visible[i] {
                    self.render(i, list.widget.selection()?)?
                }
            }
        }
        Ok(())
    }

    fn present_live(
        &mut self,
        index: usize,
        result: Result<Vec<Installed>, String>,
    ) -> liteos_ui::Result<()> {
        let next = match result {
            Ok(rows) => Live { rows, error: None },
            Err(error) => Live {
                rows: Vec::new(),
                error: Some(format!("无法读取生效路由：{error}")),
            },
        };
        let previous = &self.live[index];
        if !self.visible[index] || (previous.rows == next.rows && previous.error == next.error) {
            return Ok(());
        }
        let list = &self.lists[index];
        let selection = match list.widget.selection()? {
            Some(id) if id % 2 == 1 => previous
                .rows
                .get((id / 2) as usize)
                .and_then(|old| next.rows.iter().position(|new| old == new))
                .map(observed_id),
            selection => selection,
        };
        if self
            .observed_property
            .as_ref()
            .is_some_and(|(family, property)| {
                family_index(*family) == index && !next.rows.contains(&property.entry)
            })
        {
            self.observed_property = None;
        }
        self.live[index] = next;
        self.render(index, selection)
    }
}

pub(crate) struct Page {
    view: Rc<RefCell<View>>,
    declarations: Option<Task<()>>,
    installed: [Option<Task<()>>; 2],
    gateway: Option<Task<()>>,
    // 写请求进入配置服务后不能由界面关闭撤销；后台任务保留到提交结果返回。
    mutation: Option<Task<()>>,
    background: Background,
    clock: Clock,
}

enum OnSaved {
    List,
    Close(u64),
    Keep(u64, StaticRoute),
}

impl Page {
    pub(crate) fn new(
        app: &Application,
        parents: [&Widget; 2],
        background: Background,
    ) -> liteos_ui::Result<Self> {
        Ok(Self {
            view: Rc::new(RefCell::new(View {
                lists: [List::new(parents[0])?, List::new(parents[1])?],
                catalog: None,
                configuration_error: None,
                live: Default::default(),
                editor: None,
                observed_property: None,
                scope: None,
                visible: [false; 2],
            })),
            declarations: None,
            installed: [None, None],
            gateway: None,
            mutation: None,
            background,
            clock: app.clock(),
        })
    }

    pub(crate) fn show(
        &mut self,
        app: &mut Application,
        window: &Window,
        visible: [bool; 2],
    ) -> liteos_ui::Result<()> {
        let mut view = self.view.borrow_mut();
        if view.visible == visible {
            return Ok(());
        }
        let previous = view.visible;
        let was_open = previous.contains(&true);
        let is_open = visible.contains(&true);
        for (i, open) in visible.into_iter().enumerate() {
            if open && !previous[i] {
                if view.catalog.is_some() || view.configuration_error.is_some() {
                    view.render(i, None)?;
                } else {
                    view.lists[i].clear("正在读取路由…")?;
                }
            }
            if !open {
                view.live[i] = Live::default();
            }
        }
        view.visible = visible;
        if view
            .editor
            .as_ref()
            .is_some_and(|(family, _)| !visible[family_index(*family)])
        {
            view.editor = None;
            self.gateway = None;
        }
        if view
            .observed_property
            .as_ref()
            .is_some_and(|(family, _)| !visible[family_index(*family)])
        {
            view.observed_property = None;
        }
        if !is_open {
            view.clear("正在读取路由…")?;
        }
        drop(view);
        if was_open != is_open {
            self.observe_declarations(app, window)?;
        }
        for (i, open) in visible.into_iter().enumerate() {
            if open != previous[i] {
                self.observe_installed(app, window, i)?;
            }
        }
        Ok(())
    }

    pub(crate) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        scope: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        self.declarations = None;
        self.installed = [None, None];
        self.gateway = None;
        let mut view = self.view.borrow_mut();
        view.scope = scope;
        view.editor = None;
        view.clear("正在读取路由…")?;
        drop(view);
        self.observe_declarations(app, window)?;
        for i in 0..FAMILIES.len() {
            self.observe_installed(app, window, i)?;
        }
        Ok(())
    }

    fn observe_declarations(
        &mut self,
        app: &mut Application,
        window: &Window,
    ) -> liteos_ui::Result<()> {
        self.declarations = None;
        let view = self.view.borrow();
        if !view.visible.contains(&true) {
            return Ok(());
        }
        let scope = view.scope.clone();
        drop(view);
        let view = self.view.clone();
        let clock = self.clock.clone();
        let background = self.background.clone();
        self.declarations = Some(app.spawn_local(window, async move {
            loop {
                let started = Instant::now();
                let domain = scope.clone();
                let revision = view
                    .borrow()
                    .catalog
                    .as_ref()
                    .map(|catalog| catalog.revision);
                let result =
                    match background.run(move || Catalog::load(domain.as_deref(), revision)) {
                        Ok(task) => task.await.map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                {
                    let mut view = view.borrow_mut();
                    // 本轮读请求期间可能提交新配置；旧代际不能恢复可写列表。
                    if view.catalog.as_ref().map(|current| current.revision) == revision
                        && view.present_configuration(result).is_err()
                    {
                        break;
                    }
                }
                let request = {
                    let view = view.borrow();
                    view.catalog
                        .as_ref()
                        .map(|catalog| catalog.selection_read(view.visible))
                };
                if let Some(request) = request {
                    let revision = request.revision;
                    let result = request.read(&background, scope.clone()).await;
                    if view
                        .borrow_mut()
                        .present_selection(revision, result.as_ref())
                        .is_err()
                    {
                        break;
                    }
                }
                let Ok(sleep) = clock.sleep(Duration::from_secs(2).max(started.elapsed())) else {
                    break;
                };
                sleep.await;
            }
        })?);
        Ok(())
    }

    fn observe_installed(
        &mut self,
        app: &mut Application,
        window: &Window,
        index: usize,
    ) -> liteos_ui::Result<()> {
        self.installed[index] = None;
        let view = self.view.borrow();
        if !view.visible[index] {
            return Ok(());
        }
        let scope = view.scope.clone();
        drop(view);
        let view = self.view.clone();
        let clock = self.clock.clone();
        let background = self.background.clone();
        self.installed[index] = Some(app.spawn_local(window, async move {
            loop {
                let started = Instant::now();
                let installed = Installed::read(&background, scope.clone(), FAMILIES[index]).await;
                if view.borrow_mut().present_live(index, installed).is_err() {
                    break;
                }
                // 大表读取持续越久，留给转发/其他观察工作的间隔越长。
                let Ok(sleep) = clock.sleep(Duration::from_secs(2).max(started.elapsed())) else {
                    break;
                };
                sleep.await;
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
        let busy = self
            .mutation
            .as_ref()
            .is_some_and(|task| !task.is_finished());
        if let Some((_, property)) = &mut view.observed_property {
            if property.handle_gateway(workspace, window, event)? {
                return Ok(());
            }
            if property.closes(event) {
                view.observed_property = None;
                return Ok(());
            }
        }
        if let Some((_, editor)) = &mut view.editor
            && editor.handle_gateway(workspace, window, event)?
        {
            return Ok(());
        }
        if let Some((family, editor)) = &mut view.editor
            && !busy
            && let Some(request) = editor.request(event)?
        {
            let frame = editor.frame.id();
            let original = editor.binding.original.as_ref().map(|(key, _)| *key);
            let (change, destination) = match request {
                Request::Close => {
                    view.editor = None;
                    self.gateway = None;
                    return Ok(());
                }
                Request::Remove => {
                    let Some(key) = original else { return Ok(()) };
                    (Change::Remove(key), OnSaved::Close(frame))
                }
                Request::Save { close } => {
                    let Some((table, route)) = editor.value(*family)? else {
                        return Ok(());
                    };
                    if editor
                        .binding
                        .original
                        .as_ref()
                        .is_some_and(|(key, old)| key.table == table && old == &route)
                    {
                        if close {
                            view.editor = None;
                            self.gateway = None;
                        } else {
                            editor.note("")?;
                        }
                        return Ok(());
                    }
                    let destination = if close {
                        OnSaved::Close(frame)
                    } else {
                        OnSaved::Keep(frame, route.clone())
                    };
                    (
                        Change::Put {
                            original,
                            table,
                            route,
                        },
                        destination,
                    )
                }
            };
            let revision = editor.binding.revision;
            editor.ready(false)?;
            editor.note("正在保存…")?;
            drop(view);
            return self.submit(app, window, revision, change, destination);
        }

        for (i, list) in view.lists.iter().enumerate() {
            if !view.visible[i] {
                continue;
            }
            let find = list.find.handle(event)?;
            if matches!(find, Some(FindAction::Close)) {
                return list.find.present(&list.widget);
            }
            let catalog = view.catalog.as_ref();
            let live = &view.live[i].rows;
            let family = FAMILIES[i];
            if let Some(FindAction::Find(scan)) = find {
                let rows = list.rows(catalog, live, family)?;
                return list.find.scan_rows(
                    &list.widget,
                    rows.iter().map(|(id, text)| (*id, text.as_ref())),
                    scan,
                );
            }
            if event.node == list.table.id() && matches!(event.kind, EventKind::Select { .. }) {
                return view.render(i, list.widget.selection()?);
            }
            if event.node == list.widget.id() && matches!(event.kind, EventKind::Select { .. }) {
                return list
                    .selection_changed(list.selected(catalog, live, family)?, view.notice(i));
            }
            if !matches!(event.kind, EventKind::Activate { .. }) || busy {
                continue;
            }
            let selected = list.selected(catalog, live, family)?;
            if event.node == list.widget.id()
                && let Some(Selection::Observed(entry)) = selected
            {
                if let Some((_, property)) = &view.observed_property
                    && property.entry == *entry
                {
                    return workspace.raise(&property.frame);
                }
                view.observed_property = Some((family, ObservedProperty::new(workspace, entry)?));
                return Ok(());
            }
            let Some(catalog) = catalog else { continue };
            let entry = selected.and_then(Selection::declaration);
            let revision = catalog.revision;
            if list.remove.activated(event) {
                let Some(key) = entry.map(|entry| entry.key) else {
                    return Ok(());
                };
                drop(view);
                return self.submit(app, window, revision, Change::Remove(key), OnSaved::List);
            }
            let disable = list.disable.activated(event);
            if disable || list.enable.activated(event) {
                let Some(entry) = entry.filter(|entry| entry.route.disabled != disable) else {
                    return Ok(());
                };
                let mut route = entry.route.clone();
                route.disabled = disable;
                let change = Change::Put {
                    original: Some(entry.key),
                    table: entry.key.table,
                    route,
                };
                drop(view);
                return self.submit(app, window, revision, change, OnSaved::List);
            }
            let (original, selection) = if list.add.activated(event) {
                (None, None)
            } else if event.node == list.widget.id() {
                let Some(entry) = entry else { return Ok(()) };
                (Some((entry.key, entry.route.clone())), entry.status)
            } else {
                continue;
            };
            // 属性草稿只有一个 owner，切到另一条操作前先由用户应用或取消现有编辑。
            if let Some((_, editor)) = &view.editor {
                return workspace.raise(&editor.frame);
            }
            let default_table = list
                .table
                .selection()?
                .and_then(|id| u32::try_from(id).ok())
                .filter(|id| catalog.tables.contains(id));
            let table = original
                .as_ref()
                .map_or_else(|| default_table.unwrap_or(TABLE_MAIN), |(key, _)| key.table);
            let binding = Binding {
                revision,
                original,
                table,
            };
            let editor = Editor::new(workspace, family, &catalog.tables, binding, selection)?;
            let frame = editor.frame.id();
            view.editor = Some((family, editor));
            drop(view);
            self.gateway = Some(app.spawn_local(
                window,
                property::observe_gateway(
                    self.view.clone(),
                    frame,
                    self.background.clone(),
                    self.clock.clone(),
                ),
            )?);
            return Ok(());
        }
        Ok(())
    }

    fn submit(
        &mut self,
        app: &mut Application,
        window: &Window,
        revision: Revision,
        change: Change,
        destination: OnSaved,
    ) -> liteos_ui::Result<()> {
        let view = self.view.clone();
        let scope = view.borrow().scope.clone();
        let domain = scope.clone();
        let background = self.background.clone();
        let dialogs = app.dialogs();
        let parent = window.clone();
        self.mutation = Some(app.spawn_local(window, async move {
            let result = async {
                let (candidate, key) = background
                    .run(move || catalog::candidate(domain.as_deref(), revision, change))?
                    .await?;
                let outcome = apply(&background, candidate, revision).await?;
                Ok::<_, Box<dyn std::error::Error>>((outcome, key))
            }
            .await;
            let (saved, committed, notice) = match result {
                Ok((outcome, key)) => (
                    outcome.saved,
                    outcome.saved_revision.zip(key),
                    outcome.message,
                ),
                Err(error) => (false, None, error.to_string()),
            };
            {
                let mut view = view.borrow_mut();
                if view.scope != scope {
                    return;
                }
                if saved
                    && view
                        .catalog
                        .as_ref()
                        .is_some_and(|catalog| catalog.revision == revision)
                {
                    // 旧索引不能继续操作；观察任务随后读取新代际。
                    view.catalog = None;
                    view.configuration_error = Some("正在读取声明…".into());
                    let _ = view.render_declarations_changed();
                }
                if let Some((frame, next)) = match destination {
                    OnSaved::List => None,
                    OnSaved::Close(frame) => Some((frame, None)),
                    OnSaved::Keep(frame, route) => Some((frame, Some(route))),
                } {
                    let Some((_, editor)) = view
                        .editor
                        .as_mut()
                        .filter(|(_, open)| open.frame.id() == frame)
                    else {
                        return;
                    };
                    match (saved, next) {
                        (true, None) => view.editor = None,
                        (true, Some(route)) => match committed {
                            Some((revision, key)) => {
                                let _ = editor.bind_saved(revision, key, route);
                                let _ = editor.ready(true);
                                let _ = editor.note("");
                            }
                            None => {
                                let _ = editor.close_only();
                                let _ = editor.note("已保存。请关闭后从最新列表重新打开。");
                            }
                        },
                        (false, _) => {
                            let _ = editor.ready(true);
                            let _ = editor.note(&notice);
                        }
                    }
                    return;
                }
            }
            if !saved
                && let Ok(message) = dialogs.message(
                    &parent,
                    "路由",
                    &notice,
                    liteos_ui::MessageButtons::Ok,
                    liteos_ui::MessageIcon::Error,
                )
            {
                let _ = message.await;
            }
        })?);
        Ok(())
    }
}

fn family_index(family: Family) -> usize {
    usize::from(family == Family::Ipv6)
}
