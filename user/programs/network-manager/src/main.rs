//! LiteOS 网络管理器。诊断与配置各自使用系统已有入口。

mod backup;
mod capture;
mod configuration;
mod connections;
mod firewall;
mod neighbors;
mod observation;
mod probe;
mod routes;
mod traffic;
mod workspace;

use liteos_ui::{
    Application, Column, ControlFlow, Dialogs, EventKind, FileDialogFuture, FileDialogMode,
    FileFilter, Layout, ListOptions, ListStyle, Task, Widget, WidgetKind, WidgetSpec,
    Window, WindowOptions, state,
};
use probe::{Cancel, Kind, Probe};
use workspace::{Action as WorkspaceAction, Page, Workspace};
use std::{num::NonZeroU16, path::PathBuf, process::ExitCode};

const APP_ID: &str = "org.liteos.network-manager";

fn json_dialog(
    dialogs: &Dialogs,
    parent: &Window,
    mode: FileDialogMode,
    name: &str,
) -> liteos_ui::Result<FileDialogFuture> {
    let directory = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    dialogs.file_filtered(
        parent,
        mode,
        &directory,
        name,
        FileFilter {
            label: "路由配置 (*.json)",
            extensions: &["json"],
        },
    )
}

#[derive(Clone)]
struct Controls {
    target: Widget,
    ping: Widget,
    trace: Widget,
    port: Widget,
    upload: Widget,
    download: Widget,
    stop: Widget,
    output: Widget,
    status: Widget,
}

impl Controls {
    fn running(&self, running: bool) -> liteos_ui::Result<()> {
        self.target
            .set_state(if running { state::DISABLED } else { 0 })?;
        self.ping.set_state(if running {
            state::DISABLED
        } else {
            state::DEFAULT
        })?;
        self.trace
            .set_state(if running { state::DISABLED } else { 0 })?;
        for widget in [&self.port, &self.upload, &self.download] {
            widget.set_state(if running { state::DISABLED } else { 0 })?;
        }
        self.stop
            .set_state(if running { 0 } else { state::DISABLED })
    }
}

fn child(
    parent: &Widget,
    kind: WidgetKind,
    text: &str,
    layout: Layout,
) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}

fn run() -> liteos_ui::Result<()> {
    let mut app = Application::new()?;
    let window = app.window(WindowOptions::new("网络管理器", APP_ID, 920, 555))?;
    let grow = Layout {
        grow: 1,
        ..Layout::default()
    };
    let button = Layout {
        width: 82,
        height: 23,
        ..Layout::default()
    };
    let content = window.widget(WidgetSpec {
        layout: Layout {
            padding: 6,
            gap: 6,
            ..grow
        },
        ..WidgetSpec::new(WidgetKind::Column, "")
    })?;
    let background = app.background()?;
    let mut routing_domains =
        observation::RoutingDomains::new(&content, background.clone(), button)?;
    let mut workspace = Workspace::new(
        &content,
        window.widget(WidgetSpec::new(WidgetKind::PopupMenu, ""))?,
        grow,
    )?;
    let root = workspace.content(Page::Diagnostics)?;
    let bar = child(
        &root,
        WidgetKind::Row,
        "",
        Layout {
            gap: 6,
            ..Layout::default()
        },
    )?;
    let _label = child(&bar, WidgetKind::Label, "目标地址", Layout::default())?;
    let target = child(&bar, WidgetKind::Edit, "", grow)?;
    let ping = child(&bar, WidgetKind::Button, "Ping", button)?;
    let trace = child(&bar, WidgetKind::Button, "逐跳探测", button)?;
    let stop = child(&bar, WidgetKind::Button, "停止", button)?;
    let bandwidth = child(
        &root,
        WidgetKind::Row,
        "",
        Layout {
            gap: 6,
            ..Layout::default()
        },
    )?;
    child(
        &bandwidth,
        WidgetKind::Label,
        "带宽测试端口",
        Layout::default(),
    )?;
    let controls = Controls {
        target,
        ping,
        trace,
        stop,
        port: child(
            &bandwidth,
            WidgetKind::Edit,
            "5201",
            Layout {
                width: 58,
                ..button
            },
        )?,
        upload: child(&bandwidth, WidgetKind::Button, "测上传", button)?,
        download: child(&bandwidth, WidgetKind::Button, "测下载", button)?,
        output: child(&root, WidgetKind::List, "", grow)?,
        status: child(&root, WidgetKind::Status, "", Layout::default())?,
    };
    controls
        .output
        .set_list_style(ListStyle::Details, ListOptions::default())?;
    controls.output.set_columns(&[Column {
        id: 1,
        title: "探测结果",
        width: 630,
    }])?;
    controls.running(false)?;
    let backup_root = workspace.content(Page::Backups)?;
    let mut backups = backup::Page::new(&backup_root, app.dialogs(), background.clone(), button)?;
    let config_root = workspace.content(Page::Configuration)?;
    let mut settings =
        configuration::Page::new(&config_root, app.dialogs(), background.clone(), button)?;
    let connections_root = workspace.content(Page::Connections)?;
    let mut connections =
        connections::Page::new(&mut app, &connections_root, background.clone(), button)?;
    let firewall_root = workspace.content(Page::Firewall)?;
    let mut firewall = firewall::Page::new(&app, &firewall_root, background.clone(), button)?;
    let neighbors_root = workspace.content(Page::Neighbors)?;
    let mut neighbors = neighbors::Page::new(&app, &neighbors_root, background.clone(), button)?;
    let routes_root = workspace.content(Page::Routes)?;
    let mut routes = routes::Page::new(&app, &routes_root, background.clone(), button)?;
    let traffic_root = workspace.content(Page::Interfaces)?;
    let mut traffic = traffic::Page::new(&app, &traffic_root, background.clone(), button)?;
    workspace.open(Page::Interfaces)?;
    let mut active: Option<(Cancel, Task<()>)> = None;
    routing_domains.selected_tab(&mut app, &window, true)?;
    traffic.selected_tab(&mut app, &window, true)?;
    app.run(|app, event| {
        if event.window != window.id() {
            return Ok(ControlFlow::Continue);
        }
        if active.as_ref().is_some_and(|(_, task)| task.is_finished()) {
            active = None;
        }
        if matches!(event.kind, EventKind::Close) {
            if settings.confirm_close(app, &window)? {
                return Ok(ControlFlow::PreventDefault);
            }
            if let Some((cancel, _)) = &active {
                cancel.stop();
            }
            if traffic.finish_capture_before_close() {
                return Ok(ControlFlow::PreventDefault);
            }
            return Ok(ControlFlow::Exit);
        }
        let mut opened = None;
        if let Some(action) = workspace.action(&event)? {
            match action {
                WorkspaceAction::Open(page) => {
                    if workspace.open(page)? {
                        opened = Some(page);
                    }
                }
                WorkspaceAction::Close(page) => {
                    if page == Page::Diagnostics
                        && let Some((cancel, _)) = &active
                    {
                        cancel.stop();
                        controls.stop.set_state(state::DISABLED)?;
                        controls.status.set_text("正在停止…")?;
                    }
                    // 配置草稿随应用保留；主窗口的退出仍由原有草稿确认处理。
                    workspace.close(page)?;
                }
                WorkspaceAction::Dialog(dialog) => {
                    routes.open(app, &window, dialog)?;
                    return Ok(ControlFlow::Continue);
                }
            }
            routing_domains.selected_tab(app, &window, workspace.has_scoped_pages()?)?;
            connections.selected_tab(app, &window, workspace.is_open(Page::Connections)?)?;
            firewall.selected_tab(app, &window, workspace.is_open(Page::Firewall)?)?;
            neighbors.selected_tab(app, &window, workspace.is_open(Page::Neighbors)?)?;
            routes.selected_tab(app, &window, workspace.is_open(Page::Routes)?)?;
            traffic.selected_tab(app, &window, workspace.is_open(Page::Interfaces)?)?;
        }
        backups.handle(app, &window, &event, opened == Some(Page::Backups))?;
        settings.handle(app, &window, &event, opened == Some(Page::Configuration))?;
        if routing_domains.handle(app, &window, &event)? {
            let domain = routing_domains.selected();
            connections.select_domain(app, &window, domain.clone())?;
            firewall.select_domain(app, &window, domain.clone())?;
            neighbors.select_domain(app, &window, domain.clone())?;
            routes.select_domain(app, &window, domain.clone())?;
            traffic.select_domain(app, &window, domain)?;
        }
        connections.handle(app, &window, &event)?;
        if firewall.configure_requested(&event) {
            routes.open(app, &window, routes::Dialog::Settings(Some(routes::Section::Firewall)))?;
        }
        firewall.handle(app, &window, &event)?;
        neighbors.handle(app, &window, &event)?;
        routes.handle(app, &window, &event)?;
        if let Some(dialog) = traffic.configuration_requested(&event)? {
            routes.open(app, &window, dialog)?;
        }
        traffic.handle(app, &window, &event)?;
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(ControlFlow::Continue);
        }
        if let Some((cancel, _)) = &active {
            if event.node == controls.stop.id() {
                cancel.stop();
                controls.stop.set_state(state::DISABLED)?;
                controls.status.set_text("正在停止…")?;
            }
            return Ok(ControlFlow::Continue);
        }
        let kind = if event.node == controls.ping.id() || event.node == controls.target.id() {
            Kind::Ping
        } else if event.node == controls.trace.id() {
            Kind::Trace
        } else if event.node == controls.upload.id() || event.node == controls.download.id() {
            let Ok(port) = controls.port.text()?.trim().parse::<NonZeroU16>() else {
                controls.status.set_text("端口请输入 1 到 65535。")?;
                return Ok(ControlFlow::Continue);
            };
            Kind::Bandwidth {
                port,
                download: event.node == controls.download.id(),
            }
        } else {
            return Ok(ControlFlow::Continue);
        };
        let domain = routing_domains.selected();
        match Probe::start(
            app,
            &background,
            kind,
            &controls.target.text()?,
            domain.as_deref(),
        ) {
            Ok(probe) => {
                controls.output.set_rows(&[])?;
                controls.running(true)?;
                let scope =
                    domain.map_or_else(|| "默认路由域".to_owned(), |name| format!("「{name}」"));
                let running = if matches!(kind, Kind::Bandwidth { .. }) {
                    "正在测量带宽…"
                } else {
                    "正在探测…"
                };
                controls.status.set_text(&format!("{scope}　{running}"))?;
                let cancel = probe.cancellation();
                let controls = controls.clone();
                let task = app.spawn_local(&window, async move {
                    let text = probe
                        .display(&controls.output)
                        .await
                        .unwrap_or_else(|error| error.to_string());
                    let _ = controls.status.set_text(&format!("{scope}　{text}"));
                    let _ = controls.running(false);
                })?;
                active = Some((cancel, task));
            }
            Err(error) => controls.status.set_text(&error.to_string())?,
        }
        Ok(ControlFlow::Continue)
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("network-manager: {error}");
            ExitCode::FAILURE
        }
    }
}
