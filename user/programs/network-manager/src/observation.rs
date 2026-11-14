//! 网关观察等待复用窗口的 FD 就绪和单调时钟。

use crate::child;
use liteos_ui::{
    Application, AsyncFd, Background, Clock, Event, EventKind, Layout, Task, TreeRow, Widget,
    WidgetKind, Window, state,
};
use network_config::ManagementClient;
use std::{
    cell::RefCell,
    future::{Future, poll_fn},
    io,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::Poll,
    time::Duration,
};

pub(super) struct RoutingDomains {
    bar: Widget,
    list: Widget,
    refresh: Widget,
    status: Widget,
    names: Rc<RefCell<Vec<Arc<str>>>>,
    selected: Option<Arc<str>>,
    task: Option<Task<()>>,
    background: Background,
    visible: bool,
}

impl RoutingDomains {
    pub(super) fn new(
        parent: &Widget,
        background: Background,
        button: Layout,
    ) -> liteos_ui::Result<Self> {
        let bar = child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                gap: 6,
                ..Layout::default()
            },
        )?;
        child(&bar, WidgetKind::Label, "路由域", Layout::default())?;
        let list = child(
            &bar,
            WidgetKind::Combo,
            "",
            Layout {
                width: 218,
                ..button
            },
        )?;
        list.set_state(state::DROPDOWN_LIST)?;
        let refresh = child(&bar, WidgetKind::Button, "刷新域", button)?;
        let status = child(&bar, WidgetKind::Label, "", Layout::default())?;
        present_domains(&list, &[], None)?;
        bar.set_state(state::HIDDEN)?;
        Ok(Self {
            bar,
            list,
            refresh,
            status,
            names: Rc::default(),
            selected: None,
            task: None,
            background,
            visible: false,
        })
    }

    pub(super) fn selected_tab(
        &mut self,
        app: &mut Application,
        window: &Window,
        visible: bool,
    ) -> liteos_ui::Result<()> {
        self.visible = visible;
        self.bar
            .set_state(if visible { 0 } else { state::HIDDEN })?;
        if visible {
            self.reload(app, window)?;
        } else {
            self.task = None;
        }
        Ok(())
    }

    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        event: &Event,
    ) -> liteos_ui::Result<bool> {
        if !self.visible {
            return Ok(false);
        }
        if event.node == self.refresh.id() && matches!(event.kind, EventKind::Activate { .. }) {
            self.reload(app, window)?;
            return Ok(false);
        }
        if event.node != self.list.id()
            || !matches!(event.kind, EventKind::Select { .. })
            || self.task.as_ref().is_some_and(|task| !task.is_finished())
        {
            return Ok(false);
        }
        let Some(row) = self.list.selection()? else {
            return Ok(false);
        };
        let chosen = if row == 1 {
            None
        } else {
            let Some(name) = row
                .checked_sub(2)
                .and_then(|index| self.names.borrow().get(index as usize).cloned())
            else {
                return Ok(false);
            };
            Some(name)
        };
        let changed = chosen != self.selected;
        if changed {
            let names = self.names.borrow();
            if self
                .selected
                .as_ref()
                .is_some_and(|name| names.binary_search(name).is_err())
            {
                present_domains(&self.list, &names, chosen.as_deref())?;
                self.status.set_text("")?;
            }
            self.selected = chosen;
        }
        Ok(changed)
    }

    pub(super) fn selected(&self) -> Option<Arc<str>> {
        self.selected.clone()
    }

    fn reload(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        let load = self
            .background
            .run(|| ManagementClient::local().routing_domains())?;
        self.list
            .set_state(state::DROPDOWN_LIST | state::DISABLED)?;
        self.refresh.set_state(state::DISABLED)?;
        self.status.set_text("正在读取路由域…")?;
        let list = self.list.clone();
        let refresh = self.refresh.clone();
        let status = self.status.clone();
        let names = self.names.clone();
        let selected = self.selected.clone();
        self.task = Some(app.spawn_local(window, async move {
            let notice = match load.await {
                Ok(directory) => {
                    let current: Vec<Arc<str>> = directory.into_iter().map(Arc::from).collect();
                    let missing = selected
                        .as_ref()
                        .is_some_and(|name| current.binary_search(name).is_err());
                    if present_domains(&list, &current, selected.as_deref()).is_ok() {
                        *names.borrow_mut() = current;
                    }
                    if missing {
                        "所选路由域已删除，请选择其他域"
                    } else {
                        ""
                    }
                }
                Err(_) => "无法读取路由域",
            };
            let _ = status.set_text(notice);
            let _ = list.set_state(state::DROPDOWN_LIST);
            let _ = refresh.set_state(0);
        })?);
        Ok(())
    }
}

fn present_domains(
    list: &Widget,
    names: &[Arc<str>],
    selected: Option<&str>,
) -> liteos_ui::Result<()> {
    let labels: Vec<_> = names.iter().map(|name| format!("「{name}」")).collect();
    let mut rows: Vec<_> = std::iter::once("默认路由域")
        .chain(labels.iter().map(String::as_str))
        .enumerate()
        .map(|(index, text)| TreeRow {
            id: index as u64 + 1,
            parent: 0,
            text,
            expanded: false,
            has_children: false,
        })
        .collect();
    let missing;
    let row = match selected {
        None => 1,
        Some(name) => match names.binary_search_by(|row| row.as_ref().cmp(name)) {
            Ok(index) => index as u64 + 2,
            Err(_) => {
                // 被删除的当前域仍保留到用户明确切换，避免无提示跳回默认域。
                missing = format!("「{name}」（已删除）");
                let id = rows.len() as u64 + 1;
                rows.push(TreeRow {
                    id,
                    parent: 0,
                    text: &missing,
                    expanded: false,
                    has_children: false,
                });
                id
            }
        },
    };
    list.set_tree_rows(&rows)?;
    list.select(Some(row))
}

pub(super) async fn receive<T>(
    clock: &Clock,
    fd: &mut AsyncFd,
    timeout: Duration,
    mut read: impl FnMut() -> io::Result<Option<T>>,
) -> Result<T, Box<dyn std::error::Error>> {
    let mut deadline = clock.sleep(timeout)?;
    loop {
        if let Some(update) = read()? {
            return Ok(update);
        }
        let mut ready = fd.readable();
        poll_fn(|cx| {
            if Pin::new(&mut deadline).poll(cx).is_ready() {
                return Poll::Ready(Err(io::Error::from(io::ErrorKind::TimedOut).into()));
            }
            Pin::new(&mut ready)
                .poll(cx)
                .map(|result| result.map_err(Box::<dyn std::error::Error>::from))
        })
        .await?;
    }
}
