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

use liteos_ui::{
    Application, Column, ControlFlow, Dialogs, EventKind, FileDialogFuture, FileDialogMode,
    FileFilter, Layout, ListOptions, ListStyle, Task, TreeRow, Widget, WidgetKind, WidgetSpec,
    Window, WindowOptions, state,
};
use probe::{Cancel, Kind, Probe};
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
    let window = app.window(WindowOptions::new("网络管理器", APP_ID, 860, 488))?;
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
    let body = child(&content, WidgetKind::Row, "", grow)?;
    let navigation = child(
        &body,
        WidgetKind::Tree,
        "",
        Layout {
            width: 146,
            ..Layout::default()
        },
    )?;
    let workspace = child(&body, WidgetKind::Group, "接口", grow)?;
    let page = Layout {
        padding: 8,
        gap: 8,
        ..grow
    };
    let root = child(&workspace, WidgetKind::Column, "", page)?;
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
    let backup_root = child(&workspace, WidgetKind::Column, "", page)?;
    let mut backups = backup::Page::new(&backup_root, app.dialogs(), background.clone(), button)?;
    let config_root = child(&workspace, WidgetKind::Column, "", page)?;
    let mut settings =
        configuration::Page::new(&config_root, app.dialogs(), background.clone(), button)?;
    let connections_root = child(&workspace, WidgetKind::Column, "", page)?;
    let mut connections =
        connections::Page::new(&mut app, &connections_root, background.clone(), button)?;
    let firewall_root = child(&workspace, WidgetKind::Column, "", page)?;
    let mut firewall = firewall::Page::new(&app, &firewall_root, background.clone(), button)?;
    let neighbors_root = child(&workspace, WidgetKind::Column, "", page)?;
    let mut neighbors = neighbors::Page::new(&app, &neighbors_root, background.clone(), button)?;
    let routes_root = child(&workspace, WidgetKind::Column, "", page)?;
    let mut routes = routes::Page::new(&app, &routes_root, background.clone(), button)?;
    let traffic_root = child(&workspace, WidgetKind::Column, "", page)?;
    let mut traffic = traffic::Page::new(&app, &traffic_root, background.clone(), button)?;
    use routes::{Dialog, Section};
    // 描述导航与其关联的真实入口。子项打开所属工作区，或进入同一份事务草稿的指定设置。
    let entries = [
        (0, "接口", Some(&traffic_root), None),
        (
            0,
            "网桥",
            None,
            Some(Dialog::Settings(Some(Section::Bridges))),
        ),
        (0, "IP", None, None),
        (3, "邻居", Some(&neighbors_root), None),
        (3, "防火墙计数", Some(&firewall_root), None),
        (
            3,
            "防火墙与 NAT",
            None,
            Some(Dialog::Settings(Some(Section::Firewall))),
        ),
        (3, "转发连接", Some(&connections_root), None),
        (
            3,
            "地址分发与 DNS",
            None,
            Some(Dialog::Settings(Some(Section::AddressServices))),
        ),
        (
            3,
            "接口地址",
            None,
            Some(Dialog::Settings(Some(Section::Interfaces))),
        ),
        (0, "路由", None, None),
        (10, "路由表", Some(&routes_root), None),
        (10, "路由域与选路", None, Some(Dialog::Settings(None))),
        (
            10,
            "路由准入",
            None,
            Some(Dialog::Settings(Some(Section::Admission))),
        ),
        (10, "OSPF 邻居", None, Some(Dialog::Ospf)),
        (
            10,
            "OSPF 设置",
            None,
            Some(Dialog::Settings(Some(Section::Ospf))),
        ),
        (10, "BGP 会话", None, Some(Dialog::Bgp)),
        (
            10,
            "BGP 设置",
            None,
            Some(Dialog::Settings(Some(Section::Bgp))),
        ),
        (0, "系统", None, None),
        (18, "路由配置", Some(&config_root), None),
        (18, "配置备份", Some(&backup_root), None),
        (0, "工具", None, None),
        (21, "网络诊断", Some(&root), None),
    ];
    navigation.set_tree_rows(
        &entries
            .iter()
            .enumerate()
            .map(|(index, &(parent, text, view, dialog))| TreeRow {
                id: index as u64 + 1,
                parent,
                text,
                expanded: false,
                has_children: view.is_none() && dialog.is_none(),
            })
            .collect::<Vec<_>>(),
    )?;
    navigation.select(Some(1))?;
    let mut visible = &traffic_root;
    for (_, _, view, _) in entries {
        if let Some(view) = view
            && view.id() != visible.id()
        {
            view.set_state(state::HIDDEN)?;
        }
    }
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
        if event.node == navigation.id()
            && matches!(
                event.kind,
                EventKind::Select { .. } | EventKind::Activate { .. }
            )
            && let Some(id) = navigation.selection()?
            && let Some((_, title, view, dialog)) = entries.get((id - 1) as usize)
        {
            if let Some(view) = view
                && view.id() != visible.id()
            {
                visible.set_state(state::HIDDEN)?;
                view.set_state(0)?;
                workspace.set_text(title)?;
                visible = view;
                opened = Some(view.id());
            }
            if let Some(dialog) = dialog
                && matches!(event.kind, EventKind::Activate { .. })
            {
                routes.open(app, &window, *dialog)?;
            }
        }
        backups.handle(app, &window, &event, opened == Some(backup_root.id()))?;
        settings.handle(app, &window, &event, opened == Some(config_root.id()))?;
        if let Some(page) = opened {
            routing_domains.selected_tab(
                app,
                &window,
                [
                    root.id(),
                    connections_root.id(),
                    firewall_root.id(),
                    neighbors_root.id(),
                    routes_root.id(),
                    traffic_root.id(),
                ]
                .contains(&page),
            )?;
            connections.selected_tab(app, &window, page == connections_root.id())?;
            firewall.selected_tab(app, &window, page == firewall_root.id())?;
            neighbors.selected_tab(app, &window, page == neighbors_root.id())?;
            routes.selected_tab(app, &window, page == routes_root.id())?;
            traffic.selected_tab(app, &window, page == traffic_root.id())?;
        }
        if routing_domains.handle(app, &window, &event)? {
            let domain = routing_domains.selected();
            connections.select_domain(app, &window, domain.clone())?;
            firewall.select_domain(app, &window, domain.clone())?;
            neighbors.select_domain(app, &window, domain.clone())?;
            routes.select_domain(app, &window, domain.clone())?;
            traffic.select_domain(app, &window, domain)?;
        }
        connections.handle(app, &window, &event)?;
        firewall.handle(app, &window, &event)?;
        neighbors.handle(app, &window, &event)?;
        routes.handle(app, &window, &event)?;
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
