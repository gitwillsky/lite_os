//! 任务管理器只拥有进程/采样业务，标签页、表头、列表、菜单和对话框交互由共享 SDK 提供。
mod gpu;
mod menus;
mod performance;
mod processes;
mod sampling;
mod system;
use liteos_ui::{
    Application, ControlFlow, EventKind, Layout, Task, Toplevel, ToplevelRequest, Widget,
    WidgetKind as Kind, WidgetSpec, Window, WindowOptions, state,
};
use menus::{Command, CpuGraph, Page, Speed};
use processes::{Column, Process, Sort};
use sampling::{Refresh, Sampler};
use std::{
    cell::RefCell,
    process::{Child, Command as ProcessCommand, ExitCode, Stdio},
    rc::Rc,
};
use system::{Monitor, Totals};
const APP_ID: &str = "org.liteos.task-manager";
fn child(parent: &Widget, kind: Kind, text: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}
struct View {
    menu: Widget,
    tabs: Widget,
    pages: [Widget; 4],
    applications: Widget,
    processes: Widget,
    performance: performance::View,
    gpu: gpu::View,
    status: Widget,
    end_task: Widget,
    switch: Widget,
    new_task: Widget,
    end_process: Widget,
}
impl View {
    fn new(window: &Window) -> liteos_ui::Result<Self> {
        let mut spec = WidgetSpec::new(Kind::Column, "");
        spec.layout.grow = 1;
        let root = window.widget(spec)?;
        let menu = child(&root, Kind::MenuBar, "", Layout::default())?;
        let tabs = child(
            &root,
            Kind::Tabs,
            "",
            Layout {
                grow: 1,
                padding: 6,
                ..Default::default()
            },
        )?;
        let page = |title| {
            child(
                &tabs,
                Kind::Column,
                title,
                Layout {
                    grow: 1,
                    padding: 6,
                    gap: 6,
                    ..Default::default()
                },
            )
        };
        let pages = [
            page("应用程序")?,
            page("进程")?,
            page("性能")?,
            page("GPU")?,
        ];
        let applications = child(
            &pages[0],
            Kind::List,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        // Windows 2000 任务管理器的两张列表都是整行选择（LVS_EX_FULLROWSELECT）。
        applications.set_list_style(
            liteos_ui::ListStyle::Details,
            liteos_ui::ListOptions {
                multiple: false,
                full_row_select: true,
            },
        )?;
        applications.set_columns(&[
            liteos_ui::Column {
                id: 1,
                title: "任务",
                width: 330,
            },
            liteos_ui::Column {
                id: 2,
                title: "状态",
                width: 120,
            },
        ])?;
        let actions = child(
            &pages[0],
            Kind::Row,
            "",
            Layout {
                height: 26,
                gap: 6,
                ..Default::default()
            },
        )?;
        let button = |parent, label| {
            child(
                parent,
                Kind::Button,
                label,
                Layout {
                    width: 105,
                    height: 26,
                    ..Default::default()
                },
            )
        };
        let end_task = button(&actions, "结束任务(E)")?;
        let switch = button(&actions, "切换至(S)")?;
        let new_task = button(&actions, "新任务(N)...")?;
        let processes = child(
            &pages[1],
            Kind::List,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        // Windows 2000 任务管理器的两张列表都是整行选择（LVS_EX_FULLROWSELECT）。
        processes.set_list_style(
            liteos_ui::ListStyle::Details,
            liteos_ui::ListOptions {
                multiple: false,
                full_row_select: true,
            },
        )?;
        let process_actions = child(
            &pages[1],
            Kind::Row,
            "",
            Layout {
                height: 26,
                ..Default::default()
            },
        )?;
        let end_process = button(&process_actions, "结束进程(E)")?;
        let performance = performance::View::new(&pages[2])?;
        let gpu = gpu::View::new(&pages[3])?;
        let status = child(&root, Kind::Status, "", Layout::default())?;
        let result = Self {
            menu,
            tabs,
            pages,
            applications,
            processes,
            performance,
            gpu,
            status,
            end_task,
            switch,
            new_task,
            end_process,
        };
        result.columns()?;
        Ok(result)
    }
    fn columns(&self) -> liteos_ui::Result<()> {
        let columns: Vec<_> = Column::ALL
            .iter()
            .enumerate()
            .map(|(id, column)| liteos_ui::Column {
                id: id as u64 + 1,
                title: column.title(),
                width: column.initial_width(),
            })
            .collect();
        self.processes.set_columns(&columns)?;
        for (index, column) in Column::ALL.iter().enumerate() {
            if column.right_aligned() {
                self.processes
                    .set_column_alignment(index as u64 + 1, liteos_ui::ColumnAlignment::Right)?;
            }
        }
        Ok(())
    }
    fn processes(&self, rows: &[Process]) -> liteos_ui::Result<()> {
        let text: Vec<_> = rows
            .iter()
            .map(|row| {
                Column::ALL
                    .iter()
                    .map(|column| column.text(row))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect();
        let values: Vec<_> = rows
            .iter()
            .zip(&text)
            .map(|(row, text)| (u64::from(row.pid) + 1, text.as_str()))
            .collect();
        self.processes.set_rows(&values)?;
        if self.processes.selection()?.is_none() {
            self.processes.select(values.first().map(|(id, _)| *id))?;
        }
        Ok(())
    }
    fn applications(&self, apps: &[Toplevel]) -> liteos_ui::Result<()> {
        let text: Vec<_> = apps
            .iter()
            .map(|app| {
                format!(
                    "{}\t{}",
                    app.title.replace(['\t', '\n'], " "),
                    if app.minimized {
                        "已最小化"
                    } else {
                        "正在运行"
                    }
                )
            })
            .collect();
        let values: Vec<_> = apps
            .iter()
            .zip(&text)
            .map(|(app, text)| (app.id(), text.as_str()))
            .collect();
        self.applications.set_rows(&values)?;
        self.applications.set_row_icons(
            &apps
                .iter()
                .map(|app| liteos_ui::RowIcon {
                    id: app.id(),
                    source: liteos_ui::Icon::Application(&app.app_id),
                })
                .collect::<Vec<_>>(),
        )?;
        if self.applications.selection()?.is_none() {
            self.applications
                .select(values.first().map(|(id, _)| *id))?;
        }
        Ok(())
    }
    fn page(&self) -> liteos_ui::Result<Page> {
        let selected = self.tabs.selection()?;
        let index = self
            .pages
            .iter()
            .position(|page| Some(page.id()) == selected);
        Ok(match index {
            Some(1) => Page::Processes,
            Some(2) => Page::Performance,
            Some(3) => Page::Gpu,
            _ => Page::Applications,
        })
    }
}
struct State {
    rows: Vec<Process>,
    sort: Sort,
    speed: Speed,
    graph: CpuGraph,
    // 只读采样快照用于 resize 后重新投影曲线；canonical 采样历史由后台 Sampler 独占。
    monitor: Monitor,
    // 只读 GPU 快照用于点击列标题后按新顺序重排，不等待下一次采样。
    gpu: gpu::Snapshot,
    gpu_sort: gpu::Sort,
    children: Vec<Child>,
    applications: Vec<Toplevel>,
}
impl State {
    fn publish(
        &mut self,
        view: &View,
        monitor: &Monitor,
        mut rows: Vec<Process>,
        gpu: &gpu::Snapshot,
    ) -> liteos_ui::Result<()> {
        self.sort.apply(&mut rows);
        let totals = Totals {
            handles: rows.iter().map(|row| u64::from(row.handles)).sum(),
            threads: rows.iter().map(|row| u64::from(row.threads)).sum(),
            processes: rows.len() as u64,
        };
        view.processes(&rows)?;
        view.performance.update(monitor, totals, self.graph)?;
        view.gpu.update(gpu, self.gpu_sort)?;
        let (used, limit) = monitor.memory.commit();
        view.status.set_text(&format!(
            "进程数：{}\tCPU 使用：{}%\t认可用量：{} / {}",
            rows.len(),
            monitor.cpu_percent,
            system::memory_text(used),
            system::memory_text(limit)
        ))?;
        self.rows = rows;
        self.monitor = monitor.clone();
        self.gpu = gpu.clone();
        self.children
            .retain_mut(|child| child.try_wait().ok().flatten().is_none());
        Ok(())
    }
    fn refresh_applications(&mut self, app: &Application, view: &View) -> liteos_ui::Result<()> {
        self.applications = app
            .toplevels()?
            .into_iter()
            .filter(|entry| entry.app_id != APP_ID)
            .collect();
        view.applications(&self.applications)
    }
    fn selected_task(&self, view: &View) -> liteos_ui::Result<Option<&Toplevel>> {
        let id = view.applications.selection()?;
        Ok(self
            .applications
            .iter()
            .find(|entry| Some(entry.id()) == id))
    }
}
enum Dialog {
    NewTask {
        window: Window,
        edit: Widget,
        ok: Widget,
        cancel: Widget,
        _labels: Vec<Widget>,
    },
    About {
        window: Window,
        ok: Widget,
        _label: Widget,
    },
}
impl Dialog {
    fn window(&self) -> &Window {
        match self {
            Self::NewTask { window, .. } | Self::About { window, .. } => window,
        }
    }
    fn new(app: &mut Application, parent: &Window, about: bool) -> liteos_ui::Result<Self> {
        let mut options = WindowOptions::new(
            if about {
                "关于任务管理器"
            } else {
                "创建新任务"
            },
            APP_ID,
            420,
            210,
        );
        options.parent = Some(parent);
        options.modal = true;
        options.fixed = true;
        let window = app.window(options)?;
        let mut spec = WidgetSpec::new(Kind::Column, "");
        spec.layout = Layout {
            grow: 1,
            padding: 12,
            gap: 8,
            ..Default::default()
        };
        let root = window.widget(spec)?;
        let label = child(
            &root,
            Kind::Label,
            if about {
                "LiteOS 任务管理器\n显示应用程序、进程和系统性能。"
            } else {
                "键入程序、文件夹或文档的名称，LiteOS 将为您打开它。"
            },
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        label.set_state(state::WRAP)?;
        let edit = if about {
            None
        } else {
            Some(child(
                &root,
                Kind::Edit,
                "",
                Layout {
                    height: 26,
                    ..Default::default()
                },
            )?)
        };
        let actions = child(
            &root,
            Kind::Row,
            "",
            Layout {
                height: 26,
                gap: 8,
                ..Default::default()
            },
        )?;
        let ok = child(
            &actions,
            Kind::Button,
            "确定",
            Layout {
                width: 95,
                height: 26,
                ..Default::default()
            },
        )?;
        ok.set_state(state::DEFAULT)?;
        if let Some(edit) = edit {
            let cancel = child(
                &actions,
                Kind::Button,
                "取消",
                Layout {
                    width: 95,
                    height: 26,
                    ..Default::default()
                },
            )?;
            Ok(Self::NewTask {
                window,
                edit,
                ok,
                cancel,
                _labels: vec![label],
            })
        } else {
            Ok(Self::About {
                window,
                ok,
                _label: label,
            })
        }
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = Application::new()?;
    let window = app.window(WindowOptions::new("任务管理器", APP_ID, 610, 570))?;
    let view = Rc::new(View::new(&window)?);
    let state = Rc::new(RefCell::new(State {
        rows: Vec::new(),
        sort: Sort::default(),
        speed: Speed::Normal,
        // Windows 2000 在多处理器上默认每个 CPU 一张图。
        graph: CpuGraph::PerCpu,
        monitor: Monitor::new(),
        gpu: gpu::Snapshot::default(),
        gpu_sort: gpu::Sort::default(),
        children: Vec::new(),
        applications: Vec::new(),
    }));
    state.borrow_mut().refresh_applications(&app, &view)?;
    menus::install(
        &view.menu,
        Page::Applications,
        Speed::Normal,
        CpuGraph::PerCpu,
    )?;
    let refresh = Rc::new(Refresh::default());
    let background = app.background()?;
    let clock = app.clock();
    let sample_view = view.clone();
    let sample_state = state.clone();
    let sample_refresh = refresh.clone();
    let _sampling = app.spawn_local(&window, async move {
        let mut sampler = Sampler::new();
        let mut first = true;
        loop {
            let seen = sample_refresh.epoch();
            let work = match background.run(move || sampler.sample()) {
                Ok(work) => work,
                Err(error) => {
                    eprintln!("taskmgr: {error}");
                    break;
                }
            };
            let (next, rows) = work.await;
            sampler = next;
            if let Err(error) = sample_state.borrow_mut().publish(
                &sample_view,
                &sampler.monitor,
                rows,
                &sampler.gpu,
            ) {
                eprintln!("taskmgr: {error}");
                break;
            }
            let interval = if first {
                first = false;
                Some(300)
            } else {
                sample_state.borrow().speed.interval_ms()
            };
            if let Err(error) = sample_refresh.wait(&clock, seen, interval).await {
                eprintln!("taskmgr: {error}");
                break;
            }
        }
    })?;
    let mut dialog: Option<Dialog> = None;
    let mut operation: Option<Task<()>> = None;
    app.run(|app, event| {
        if matches!(event.kind, EventKind::ToplevelsChanged) {
            state.borrow_mut().refresh_applications(app, &view)?;
            return Ok(ControlFlow::Continue);
        }
        if dialog
            .as_ref()
            .is_some_and(|dialog| dialog.window().downgrade().upgrade().is_none())
        {
            dialog = None;
        }
        if event.window != window.id() {
            if let Some(active) = &dialog {
                if active.window().id() != event.window {
                    return Ok(ControlFlow::Continue);
                }
                let mut close = matches!(
                    event.kind,
                    EventKind::Close
                        | EventKind::Key {
                            code: 1,
                            pressed: true,
                            handled: false,
                            ..
                        }
                );
                match (active, &event.kind) {
                    (Dialog::About { ok, .. }, EventKind::Activate { .. })
                        if event.node == ok.id() =>
                    {
                        close = true
                    }
                    (Dialog::NewTask { cancel, .. }, EventKind::Activate { .. })
                        if event.node == cancel.id() =>
                    {
                        close = true
                    }
                    (
                        Dialog::NewTask {
                            edit,
                            ok,
                            window: prompt,
                            ..
                        },
                        EventKind::Activate { .. },
                    ) if event.node == ok.id() => {
                        let command = edit.text()?;
                        if !command.trim().is_empty() {
                            ok.set_state(state::DISABLED)?;
                            let started = app.blocking(move || {
                                ProcessCommand::new("/bin/sh")
                                    .args(["-c", &command])
                                    .env_remove("WAYLAND_SOCKET")
                                    .stdin(Stdio::null())
                                    .stdout(Stdio::null())
                                    .spawn()
                            })?;
                            let state = state.clone();
                            let status = view.status.clone();
                            let prompt = prompt.clone();
                            let refresh = refresh.clone();
                            operation = Some(app.spawn_local(&window, async move {
                                match started.await {
                                    Ok(child) => {
                                        state.borrow_mut().children.push(child);
                                        refresh.request();
                                    }
                                    Err(error) => {
                                        let _ = status.set_text(&format!("无法启动任务：{error}"));
                                    }
                                }
                                let _ = prompt.close();
                            })?);
                        }
                    }
                    _ => {}
                }
                if close && let Some(dialog) = dialog.take() {
                    dialog.window().close()?;
                }
            }
            return Ok(ControlFlow::Continue);
        }
        let mut command = None;
        let mut end = false;
        let mut switch = false;
        match event.kind {
            EventKind::Select { .. } if event.node == view.tabs.id() => {
                let state = state.borrow();
                menus::install(&view.menu, view.page()?, state.speed, state.graph)?;
                view.performance.charts(&state.monitor, state.graph)?;
            }
            EventKind::Configure { .. } => {
                let state = state.borrow();
                view.performance.charts(&state.monitor, state.graph)?
            }
            EventKind::Column { column } if event.node == view.gpu.clients().id() => {
                if let Some(column) = gpu::Column::ALL.get(column.saturating_sub(1) as usize) {
                    let mut state = state.borrow_mut();
                    state.gpu_sort.click(*column);
                    view.gpu.update(&state.gpu, state.gpu_sort)?;
                }
            }
            EventKind::Column { column } if event.node == view.processes.id() => {
                if let Some(column) = Column::ALL.get(column.saturating_sub(1) as usize) {
                    let mut state = state.borrow_mut();
                    state.sort.click(*column);
                    let sort = state.sort;
                    sort.apply(&mut state.rows);
                    view.processes(&state.rows)?;
                }
            }
            EventKind::Activate { value } if event.node == view.menu.id() => {
                command = Command::decode(value)
            }
            EventKind::Activate { .. } if event.node == view.new_task.id() => {
                command = Some(Command::NewTask)
            }
            EventKind::Activate { .. }
                if event.node == view.end_task.id() || event.node == view.end_process.id() =>
            {
                end = true
            }
            EventKind::Activate { .. }
                if event.node == view.switch.id() || event.node == view.applications.id() =>
            {
                switch = true
            }
            EventKind::Key {
                code: 1,
                pressed: true,
                handled: false,
                ..
            } => return Ok(ControlFlow::Exit),
            EventKind::Key {
                code: 63,
                pressed: true,
                handled: false,
                ..
            } => command = Some(Command::Refresh),
            EventKind::Key {
                code: 111,
                pressed: true,
                handled: false,
                ..
            } => end = true,
            _ => {}
        }
        if end {
            if view.page()? == Page::Applications {
                if let Some(task) = state.borrow().selected_task(&view)?
                    && let Err(error) = task.request(ToplevelRequest::Close)
                {
                    view.status.set_text(&error.to_string())?;
                }
            } else if view.page()? == Page::Processes
                && let Some(id) = view.processes.selection()?
            {
                let pid = id.saturating_sub(1) as u32;
                let work = app.blocking(move || processes::terminate(pid))?;
                let status = view.status.clone();
                let refresh = refresh.clone();
                operation = Some(app.spawn_local(&window, async move {
                    if !work.await {
                        let _ = status.set_text("无法结束进程；检查权限或进程是否仍存在。");
                    }
                    refresh.request();
                })?);
            }
        }
        if switch
            && let Some(task) = state.borrow().selected_task(&view)?
            && let Err(error) = task.request(ToplevelRequest::Activate)
        {
            view.status.set_text(&error.to_string())?;
        }
        match command {
            Some(Command::Exit) => return Ok(ControlFlow::Exit),
            Some(Command::NewTask | Command::About) => {
                if let Some(old) = dialog.take() {
                    old.window().close()?;
                }
                dialog = Some(Dialog::new(app, &window, command == Some(Command::About))?);
            }
            Some(Command::Refresh) => refresh.request(),
            Some(Command::SetSpeed(speed)) => {
                state.borrow_mut().speed = speed;
                menus::install(&view.menu, view.page()?, speed, state.borrow().graph)?;
                refresh.request();
            }
            Some(Command::SetCpuGraph(graph)) => {
                let mut state = state.borrow_mut();
                state.graph = graph;
                menus::install(&view.menu, view.page()?, state.speed, graph)?;
                view.performance.charts(&state.monitor, graph)?;
            }
            Some(command @ (Command::Minimize | Command::Maximize | Command::BringToFront)) => {
                if let Some(task) = state.borrow().selected_task(&view)? {
                    let action = match command {
                        Command::Minimize => ToplevelRequest::Minimize(true),
                        Command::Maximize => ToplevelRequest::Maximize(true),
                        _ => ToplevelRequest::Activate,
                    };
                    if let Err(error) = task.request(action) {
                        view.status.set_text(&error.to_string())?;
                    }
                }
            }
            None => {}
        }
        Ok(ControlFlow::Continue)
    })?;
    drop(operation);
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("taskmgr: {error}");
            ExitCode::FAILURE
        }
    }
}
