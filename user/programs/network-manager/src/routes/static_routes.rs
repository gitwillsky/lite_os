//! 「IP / IPv6 → 路由」的静态配置工作区。双栈窗口共用一份配置投影；原生服务仍负责校验、持久化和生效。

mod catalog;
mod property;

use self::{
    catalog::{Catalog, Change, Entry, table_name},
    property::{Binding, Editor, Request},
};
use crate::{
    child,
    configuration::apply,
    item_list::{Glyph, Item, Summary, Tool, spacer, toolbar},
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
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
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
    find: Tool,
    query: Widget,
    table: Widget,
    summary: Summary,
    // SDK 不暴露行数；保留筛选后的可见计数，选中变化时无需重建列表。
    displayed: Cell<usize>,
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
        let [find] = toolbar(&bar, [Item::Text("查找")])?;
        let query = child(
            &bar,
            WidgetKind::Edit,
            "",
            Layout {
                width: 140,
                ..Layout::default()
            },
        )?;
        query.set_state(state::HIDDEN)?;
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
            query,
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
        self.displayed.set(0);
        self.selection_changed(None)?;
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
        catalog: &Catalog,
        family: Family,
        new_revision: bool,
    ) -> liteos_ui::Result<()> {
        if new_revision {
            self.set_tables(&catalog.tables)?;
        }
        let table = self.table.selection()?.unwrap_or(ALL_TABLES);
        let query = self.query.text()?;
        let search = query.trim();
        let rows: Vec<_> = catalog
            .entries(family)
            .iter()
            .enumerate()
            .filter(|(_, entry)| table == ALL_TABLES || table == u64::from(entry.key.table))
            .filter(|(_, entry)| entry.text.contains(search))
            .map(|(i, entry)| (i as u64 + 1, entry.text.as_str()))
            .collect();
        let selection = if new_revision {
            None
        } else {
            self.widget.selection()?
        };
        self.widget.set_rows(&rows)?;
        self.widget
            .select(selection.filter(|id| rows.iter().any(|row| row.0 == *id)))?;
        self.add.set_enabled(true)?;
        self.displayed.set(rows.len());
        self.selection_changed(self.selected(catalog, family)?)
    }

    fn selected<'a>(
        &self,
        catalog: &'a Catalog,
        family: Family,
    ) -> liteos_ui::Result<Option<&'a Entry>> {
        Ok(self
            .widget
            .selection()?
            .and_then(|id| id.checked_sub(1))
            .and_then(|index| catalog.entries(family).get(index as usize)))
    }

    fn selection_changed(&self, entry: Option<&Entry>) -> liteos_ui::Result<()> {
        let selected = entry.is_some();
        self.enable
            .set_enabled(entry.is_some_and(|entry| entry.route.disabled))?;
        self.disable
            .set_enabled(entry.is_some_and(|entry| !entry.route.disabled))?;
        self.remove.set_enabled(selected)?;
        self.summary
            .show(self.displayed.get(), usize::from(selected), "实时")
    }

    fn toggle_find(&self) -> liteos_ui::Result<()> {
        if self.query.state()? & state::HIDDEN != 0 {
            self.query.set_state(0)?;
            self.query.focus()
        } else {
            self.query.set_text("")?;
            self.query.set_state(state::HIDDEN)
        }
    }
}

struct View {
    lists: [List; 2],
    catalog: Option<Catalog>,
    editor: Option<(Family, Editor)>,
    scope: Option<Arc<str>>,
    visible: [bool; 2],
}

impl View {
    fn clear(&mut self, notice: &str) -> liteos_ui::Result<()> {
        self.catalog = None;
        for (list, visible) in self.lists.iter().zip(self.visible) {
            if visible {
                list.clear(notice)?;
            }
        }
        Ok(())
    }

    fn present(&mut self, catalog: Catalog) -> liteos_ui::Result<()> {
        for (i, family) in FAMILIES.into_iter().enumerate() {
            if self.visible[i] {
                self.lists[i].render(&catalog, family, true)?;
            }
        }
        self.catalog = Some(catalog);
        Ok(())
    }
}

pub(crate) struct Page {
    view: Rc<RefCell<View>>,
    observation: Option<Task<()>>,
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
                editor: None,
                scope: None,
                visible: [false; 2],
            })),
            observation: None,
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
        let was_open = view.visible.contains(&true);
        let is_open = visible.contains(&true);
        for (i, family) in FAMILIES.into_iter().enumerate() {
            if visible[i] && !view.visible[i] {
                if let Some(catalog) = &view.catalog {
                    view.lists[i].render(catalog, family, true)?;
                } else {
                    view.lists[i].clear("正在读取路由…")?;
                }
            }
        }
        view.visible = visible;
        if view
            .editor
            .as_ref()
            .is_some_and(|(family, _)| !visible[family_index(*family)])
        {
            view.editor = None;
        }
        drop(view);
        if was_open != is_open {
            self.observe(app, window)?;
        }
        Ok(())
    }

    pub(crate) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        scope: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        let mut view = self.view.borrow_mut();
        view.scope = scope;
        view.editor = None;
        drop(view);
        self.observe(app, window)
    }

    fn observe(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        self.observation = None;
        let mut view = self.view.borrow_mut();
        view.clear("正在读取路由…")?;
        if !view.visible.contains(&true) {
            return Ok(());
        }
        let scope = view.scope.clone();
        drop(view);
        let view = self.view.clone();
        let clock = self.clock.clone();
        let background = self.background.clone();
        self.observation = Some(app.spawn_local(window, async move {
            loop {
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
                let shown = match result {
                    Ok(Some(catalog)) => view.borrow_mut().present(catalog),
                    Ok(None) => Ok(()),
                    Err(error) => view
                        .borrow_mut()
                        .clear(&format!("无法读取路由配置：{error}")),
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
        if let Some((family, editor)) = &view.editor
            && !busy
            && let Some(request) = editor.request(event)?
        {
            let frame = editor.frame.id();
            let original = editor.binding.original.as_ref().map(|(key, _)| *key);
            let (change, destination) = match request {
                Request::Close => {
                    view.editor = None;
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
            let find = list.find.activated(event);
            if find {
                list.toggle_find()?;
            }
            let Some(catalog) = &view.catalog else {
                continue;
            };
            let family = FAMILIES[i];
            let input = (event.node == list.query.id()
                && matches!(event.kind, EventKind::Changed { .. }))
                || (event.node == list.table.id()
                    && matches!(event.kind, EventKind::Select { .. }));
            if find || input {
                return list.render(catalog, family, false);
            }
            if event.node == list.widget.id() && matches!(event.kind, EventKind::Select { .. }) {
                return list.selection_changed(list.selected(catalog, family)?);
            }
            if !matches!(event.kind, EventKind::Activate { .. }) || busy {
                continue;
            }
            let entry = list.selected(catalog, family)?;
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
            let original = if list.add.activated(event) {
                None
            } else if event.node == list.widget.id() {
                let Some(entry) = entry else { return Ok(()) };
                Some((entry.key, entry.route.clone()))
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
                .and_then(|id| u32::try_from(id).ok());
            let table = original
                .as_ref()
                .map_or_else(|| default_table.unwrap_or(TABLE_MAIN), |(key, _)| key.table);
            let binding = Binding {
                revision,
                original,
                table,
            };
            view.editor = Some((
                family,
                Editor::new(workspace, family, &catalog.tables, binding)?,
            ));
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
                    let _ = view.clear("正在读取路由…");
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
                                let _ = editor.note(&notice);
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
