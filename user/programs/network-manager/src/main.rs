//! LiteOS 网络管理器。诊断与配置各自使用系统已有入口。

mod backup;
mod capture;
mod configuration;
mod connections;
mod diagnostics;
mod firewall;
mod interface_properties;
mod item_list;
mod navigation;
mod neighbors;
mod observation;
mod probe;
mod routes;
mod system_status;
mod traffic;
mod workspace;

use liteos_ui::{
    Application, ControlFlow, Dialogs, EventKind, FileDialogFuture, FileDialogMode, FileFilter,
    Layout, MessageButtons, MessageIcon, Task, Widget, WidgetKind, WidgetSpec, Window,
    WindowOptions,
};
use navigation::{Action, Navigation};
use std::{
    path::PathBuf,
    process::{Child, Command, ExitCode, Stdio},
};
use workspace::{Page, Workspace};

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

/// WinBox 标题栏的「用户@主机 - 程序名」。
fn title() -> String {
    let user = std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| "root".into());
    let host = linux_uapi::uts::read().map_or_else(
        |_| "localhost".into(),
        |identity| identity.hostname.to_string_lossy().into_owned(),
    );
    format!("{user}@{host} - 网络管理器")
}

/// 「新终端」按 Windows 2000 进程模型启动独立的系统终端程序，网络管理器只负责回收它。
fn launch_terminal() -> std::io::Result<Child> {
    Command::new("/bin/terminal")
        .env_remove("WAYLAND_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
}

fn run() -> liteos_ui::Result<()> {
    let mut app = Application::new()?;
    let window = app.window(WindowOptions::new(&title(), APP_ID, 1180, 720))?;
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
        layout: Layout { gap: 0, ..grow },
        ..WidgetSpec::new(WidgetKind::Column, "")
    })?;
    // 1. 顶栏：路由域决定各观察窗口的查询范围。
    let top = child(
        &content,
        WidgetKind::Row,
        "",
        Layout {
            padding: 3,
            gap: 6,
            ..Layout::default()
        },
    )?;
    let background = app.background()?;
    let mut routing_domains =
        observation::RoutingDomains::new(&mut app, &window, &top, background.clone())?;
    // 2. 主体：左侧竖排品牌条和菜单，右侧多窗口工作区。
    let body = child(&content, WidgetKind::Row, "", Layout { gap: 0, ..grow })?;
    let mut navigation = Navigation::new(
        &body,
        window.widget(WidgetSpec::new(WidgetKind::PopupMenu, ""))?,
    )?;
    let workspace = Workspace::new(&body, grow)?;
    // 3. 底部状态栏：主机与系统版本、CPU、内存、运行时间和本地时间。
    let _system_status =
        system_status::SystemStatus::new(&mut app, &window, &content, background.clone())?;
    let mut diagnostics =
        diagnostics::Page::new(&workspace.content(Page::Diagnostics)?, background.clone())?;
    let backup_root = workspace.content(Page::Backups)?;
    let mut backups = backup::Page::new(&backup_root, app.dialogs(), background.clone(), button)?;
    let config_root = workspace.content(Page::Configuration)?;
    let mut settings =
        configuration::Page::new(&config_root, app.dialogs(), background.clone(), button)?;
    let connections_root = workspace.content(Page::Connections)?;
    let mut connections = connections::Page::new(&mut app, &connections_root, background.clone())?;
    let firewall_root = workspace.content(Page::Firewall)?;
    let mut firewall = firewall::Page::new(&app, &firewall_root, background.clone())?;
    let neighbors_root = workspace.content(Page::Neighbors)?;
    let mut neighbors = neighbors::Page::new(&app, &neighbors_root, background.clone())?;
    let routes_root = workspace.content(Page::Routes)?;
    let mut routes = routes::Page::new(&app, &routes_root, background.clone())?;
    let traffic_root = workspace.content(Page::Interfaces)?;
    let mut traffic = traffic::Page::new(&app, &traffic_root, background.clone())?;
    let mut interfaces = interface_properties::InterfaceWindows::new(&app, background.clone());
    workspace.open(Page::Interfaces)?;
    traffic.selected_tab(&mut app, &window, true)?;
    // 已启动的终端在退出后回收，避免留下僵尸进程。
    let mut terminals: Vec<Child> = Vec::new();
    // 启动失败的提示框；丢弃任务会取消提示。
    let mut notice: Option<Task<()>> = None;
    app.run(|app, event| {
        if event.window != window.id() {
            return Ok(ControlFlow::Continue);
        }
        terminals.retain_mut(|terminal| matches!(terminal.try_wait(), Ok(None)));
        let mut exit = matches!(event.kind, EventKind::Close);
        let mut opened = None;
        let mut changed = false;
        match navigation.handle(&event)? {
            Some(Action::Open(page)) => {
                if workspace.open(page)? {
                    opened = Some(page);
                }
                changed = true;
            }
            Some(Action::Dialog(dialog)) => {
                routes.open(app, &window, dialog)?;
                return Ok(ControlFlow::Continue);
            }
            Some(Action::Terminal) => {
                match launch_terminal() {
                    Ok(terminal) => terminals.push(terminal),
                    Err(error) => {
                        let message = app.dialogs().message(
                            &window,
                            "新终端",
                            &format!("无法启动终端：{error}"),
                            MessageButtons::Ok,
                            MessageIcon::Error,
                        )?;
                        notice = Some(app.spawn_local(&window, async move {
                            let _ = message.await;
                        })?);
                    }
                }
                return Ok(ControlFlow::Continue);
            }
            Some(Action::Exit) => exit = true,
            None => {}
        }
        if exit {
            if settings.confirm_close(app, &window)? {
                return Ok(ControlFlow::PreventDefault);
            }
            diagnostics.stop()?;
            if traffic.finish_capture_before_close() {
                return Ok(ControlFlow::PreventDefault);
            }
            return Ok(ControlFlow::Exit);
        }
        if let Some(page) = workspace.closed(&event) {
            if page == Page::Diagnostics {
                diagnostics.stop()?;
            }
            // 配置草稿随应用保留；主窗口的退出仍由原有草稿确认处理。
            workspace.close(page)?;
            changed = true;
        }
        if changed {
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
            routes.open(
                app,
                &window,
                routes::Dialog::Settings(Some(routes::Section::Firewall)),
            )?;
        }
        firewall.handle(app, &window, &event)?;
        neighbors.handle(app, &window, &event)?;
        routes.handle(app, &window, &event)?;
        match traffic.request(&event)? {
            Some(traffic::Request::Dialog(dialog)) => routes.open(app, &window, dialog)?,
            Some(traffic::Request::Properties(row)) => {
                interfaces.open(app, &window, &workspace, routing_domains.selected(), row)?;
            }
            Some(traffic::Request::SetEnabled(row, enabled)) => {
                interfaces.set_enabled(app, &window, routing_domains.selected(), row, enabled)?;
            }
            None => {}
        }
        interfaces.handle(app, &window, &event, routing_domains.selected())?;
        interfaces.prune();
        traffic.handle(app, &window, &event)?;
        if workspace.is_open(Page::Diagnostics)? {
            diagnostics.handle(app, &window, &event, routing_domains.selected())?;
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
