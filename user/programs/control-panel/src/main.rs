//! 控制面板：属性表独占共同交互，各页只提供数据、校验、领域写入与必要预览。
mod connection;
mod datetime;
mod display;
mod fileprops;
mod ip_address;
mod network;
mod sounds;
mod system;
mod taskbar;
mod winver;
mod zones;
use liteos_ui::{
    Application, Background, Clock, ControlFlow, Event, EventKind, Layout, MessageButtons,
    PropertySheet, ValidationError, Widget, WidgetKind as Kind, WidgetSpec,
};
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    process::{Child, Command, ExitCode},
    rc::Rc,
    task::Waker,
    time::Duration,
};
type Apply = Box<dyn FnOnce() -> Result<(), String> + Send>;
type Activity = Pin<Box<dyn Future<Output = ()>>>;
/// 页面对输入事件的响应。
enum Response {
    /// 启动独立程序（程序与参数）。
    Launch(Vec<String>),
    /// 关闭属性表（状态窗口的“禁用”）。
    Close,
    /// 在 UI 线程运行的异步流程（模态子对话框）；结束后重新计算属性表的“已修改”状态。
    Task(Activity),
}
/// 一个属性页集合的业务接口；控件状态由运行库持有，saved 仅用于识别尚未应用的领域草稿。
trait Page {
    fn event(&mut self, _event: &Event) -> liteos_ui::Result<Option<Response>> {
        Ok(None)
    }
    fn dirty(&self) -> liteos_ui::Result<bool> {
        Ok(false)
    }
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
    fn apply(&self) -> liteos_ui::Result<Apply> {
        Ok(Box::new(|| Ok(())))
    }
    fn committed(&mut self) -> liteos_ui::Result<()> {
        Ok(())
    }
    fn activity(&self, _background: Background, _clock: Clock) -> Option<Activity> {
        None
    }
}
fn child(parent: &Widget, kind: Kind, text: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}
fn label(parent: &Widget, text: &str) -> liteos_ui::Result<Widget> {
    let widget = child(parent, Kind::Label, text, Layout::default())?;
    widget.set_state(liteos_ui::state::WRAP)?;
    Ok(widget)
}
fn image(parent: &Widget, stem: &str) -> liteos_ui::Result<Widget> {
    child(
        parent,
        Kind::Image,
        stem,
        Layout {
            width: 32,
            height: 32,
            ..Default::default()
        },
    )
}
fn group(parent: &Widget, title: &str) -> liteos_ui::Result<Widget> {
    child(
        parent,
        Kind::Group,
        title,
        Layout {
            padding: 10,
            gap: 5,
            ..Default::default()
        },
    )
}
fn pair(parent: &Widget, title: &str, text: &str) -> liteos_ui::Result<(Widget, Widget)> {
    let row = child(
        parent,
        Kind::Row,
        "",
        Layout {
            gap: 8,
            ..Default::default()
        },
    )?;
    let heading = child(
        &row,
        Kind::Label,
        title,
        Layout {
            width: 100,
            ..Default::default()
        },
    )?;
    let value = child(
        &row,
        Kind::Label,
        text,
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    Ok((heading, value))
}
enum Data {
    System(system::Data),
    Network(network::Data),
    Connection(String),
    Properties(fileprops::Data),
    Settings(desktop_settings::Settings),
    Sounds(bool),
    Other,
}
fn load(name: &str, args: Vec<std::ffi::OsString>) -> Result<Data, String> {
    Ok(match name {
        "system" => Data::System(system::Data::read()),
        "network" => Data::Network(network::Data::read(
            args.into_iter()
                .next()
                .map(|name| name.to_string_lossy().into_owned()),
        )?),
        "connection" => Data::Connection(
            args.into_iter()
                .next()
                .map(|name| name.to_string_lossy().into_owned())
                .or_else(network::first_ethernet)
                .ok_or("没有网络连接")?,
        ),
        "properties" => Data::Properties(fileprops::Data::read(args.into_iter())?),
        "sounds" => Data::Sounds(std::path::Path::new("/dev/snd/pcmC0D0p").exists()),
        "display" | "taskbar" => Data::Settings(desktop_settings::Settings::load()),
        _ => Data::Other,
    })
}
fn build(
    name: &str,
    data: Data,
    sheet: &PropertySheet,
    dialogs: &liteos_ui::Dialogs,
) -> liteos_ui::Result<Box<dyn Page>> {
    Ok(match (name, data) {
        ("connection", Data::Connection(interface)) => Box::new(connection::Connection::build(
            sheet,
            interface,
            dialogs.clone(),
        )?),
        ("system", Data::System(data)) => Box::new(system::System::build(sheet, data)?),
        ("network", Data::Network(data)) => Box::new(network::Network::build(sheet, data)?),
        ("properties", Data::Properties(data)) => {
            Box::new(fileprops::FileProperties::build(sheet, data)?)
        }
        ("display", Data::Settings(data)) => Box::new(display::Display::build(sheet, data)?),
        ("taskbar", Data::Settings(data)) => Box::new(taskbar::Taskbar::build(sheet, data)?),
        ("datetime", _) => Box::new(datetime::DateTime::build(sheet)?),
        ("sounds", Data::Sounds(available)) => Box::new(sounds::Sounds::build(sheet, available)?),
        _ => Box::new(winver::Winver::build(sheet)?),
    })
}
#[derive(Default)]
struct Launches {
    children: Vec<Child>,
    wake: Option<Waker>,
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let name = std::env::args().nth(1).ok_or("missing applet")?;
    let (title, width, height) = match name.as_str() {
        // netshell 对话框 16004（227×217 DLU）加属性表边框。
        "connection" => ("本地连接 属性", 370, 400),
        "system" => ("系统属性", 420, 470),
        // 对话框 252×146 DLU 的页面加属性表的标签页、按钮与边框。
        "datetime" => ("日期/时间 属性", 410, 330),
        "sounds" => ("声音和多媒体 属性", 440, 470),
        "taskbar" => ("任务栏属性", 420, 340),
        "display" => ("显示 属性", 420, 450),
        // netshell 对话框 1801（210×141 DLU）加属性表边框。
        "network" => ("本地连接 状态", 345, 320),
        "properties" => ("属性", 420, 490),
        "winver" => ("关于 LiteOS", 420, 340),
        _ => return Err(format!("unknown applet {name}").into()),
    };
    let mut app = Application::new()?;
    // 状态窗口只有“关闭”，连接属性只有“确定/取消”（netshell 的 PSM_CANCELTOCLOSE 与 PSH_NOAPPLYNOW）。
    let style = match name.as_str() {
        "network" => liteos_ui::SheetStyle::CloseOnly,
        "connection" => liteos_ui::SheetStyle::NoApply,
        _ => liteos_ui::SheetStyle::Standard,
    };
    let sheet = Rc::new(PropertySheet::new(
        &mut app,
        None,
        title,
        (width, height),
        style,
    )?);
    let window = sheet.window().clone();
    window.set_app_id(&format!("org.liteos.control-panel.{name}"))?;
    let pending = sheet.page("正在读取")?;
    let message = label(&pending, "正在读取系统信息…")?;
    let model: Rc<RefCell<Option<Box<dyn Page>>>> = Rc::new(RefCell::new(None));
    let background = app.background()?;
    let dialogs = app.dialogs();
    let mut tasks = Vec::new();
    let work = background.run({
        let name = name.clone();
        let args = std::env::args_os().skip(2).collect();
        move || load(&name, args)
    })?;
    let startup = {
        let sheet = sheet.clone();
        let dialogs = dialogs.clone();
        let model = model.clone();
        let clock = app.clock();
        let background = background.clone();
        app.spawn_local(&window, async move {
            let result = match work.await {
                Ok(data) => build(&name, data, &sheet, &dialogs),
                Err(error) => {
                    let _ = message.set_text(&error);
                    return;
                }
            };
            match result {
                Ok(page) => {
                    drop(message);
                    drop(pending);
                    let activity = page.activity(background, clock);
                    *model.borrow_mut() = Some(page);
                    let _ = sheet.set_dirty(false);
                    if let Some(activity) = activity {
                        activity.await;
                    }
                }
                Err(error) => {
                    let _ = message.set_text(&error.to_string());
                }
            }
        })?
    };
    let launches = Rc::new(RefCell::new(Launches::default()));
    let _reaper = {
        let launches = launches.clone();
        let clock = app.clock();
        app.spawn_local(&window, async move {
            loop {
                std::future::poll_fn(|cx| {
                    let mut launches = launches.borrow_mut();
                    if launches.children.is_empty() {
                        launches.wake = Some(cx.waker().clone());
                        std::task::Poll::Pending
                    } else {
                        std::task::Poll::Ready(())
                    }
                })
                .await;
                launches
                    .borrow_mut()
                    .children
                    .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
                if let Ok(wait) = clock.sleep(Duration::from_secs(1)) {
                    wait.await;
                } else {
                    break;
                }
            }
        })?
    };
    app.run(|app, event| {
        tasks.retain(|task: &liteos_ui::Task<()>| !task.is_finished());
        if event.window != window.id() {
            return Ok(ControlFlow::Continue);
        }
        let mut page_borrow = model.borrow_mut();
        let Some(page) = page_borrow.as_mut() else {
            return Ok(ControlFlow::Continue);
        };
        match event.kind {
            EventKind::Validate { token } => sheet.validated(token, page.validate())?,
            EventKind::Apply { token } => {
                let work = background.run(page.apply()?)?;
                let sheet = sheet.clone();
                let model = model.clone();
                drop(page_borrow);
                tasks.push(app.spawn_local(&window, async move {
                    let mut result = work.await;
                    if result.is_ok()
                        && let Some(page) = model.borrow_mut().as_mut()
                    {
                        result = page.committed().map_err(|error| error.to_string());
                    }
                    let _ = sheet.applied(token, result);
                })?);
            }
            _ if matches!(
                event.kind,
                EventKind::Changed { .. }
                    | EventKind::Activate { .. }
                    | EventKind::Select { .. }
                    | EventKind::Configure { .. }
            ) =>
            {
                let report = {
                    let dialogs = dialogs.clone();
                    let window = window.clone();
                    move |message: String| {
                        let dialogs = dialogs.clone();
                        let window = window.clone();
                        async move {
                            if let Ok(dialog) =
                                dialogs.message(&window, "控制面板", &message, MessageButtons::Ok)
                            {
                                let _ = dialog.await;
                            }
                        }
                    }
                };
                match page.event(&event) {
                    Ok(Some(Response::Launch(command))) => {
                        let work = background.run(move || {
                            let mut command_builder = Command::new(&command[0]);
                            command_builder
                                .args(&command[1..])
                                .env_remove("WAYLAND_SOCKET")
                                .spawn()
                        })?;
                        let launches = launches.clone();
                        let report = report.clone();
                        tasks.push(app.spawn_local(sheet.window(), async move {
                            match work.await {
                                Ok(child) => {
                                    let wake = {
                                        let mut launches = launches.borrow_mut();
                                        launches.children.push(child);
                                        launches.wake.take()
                                    };
                                    if let Some(wake) = wake {
                                        wake.wake();
                                    }
                                }
                                Err(error) => report(error.to_string()).await,
                            }
                        })?);
                    }
                    Ok(Some(Response::Close)) => {
                        drop(page_borrow);
                        window.close()?;
                        return Ok(ControlFlow::Continue);
                    }
                    Ok(Some(Response::Task(task))) => {
                        // 子对话框可能改了草稿：结束后按页面重新计算“已修改”，使“确定”写回。
                        let sheet = sheet.clone();
                        let model = model.clone();
                        tasks.push(app.spawn_local(&window, async move {
                            task.await;
                            if let Some(page) = model.borrow().as_ref()
                                && let Ok(dirty) = page.dirty()
                            {
                                let _ = sheet.set_dirty(dirty);
                            }
                        })?);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tasks.push(app.spawn_local(sheet.window(), report(error.to_string()))?);
                    }
                }
                if !matches!(event.kind, EventKind::Configure { .. }) {
                    sheet.set_dirty(page.dirty()?)?;
                }
            }
            _ => {}
        }
        Ok(ControlFlow::Continue)
    })?;
    drop(startup);
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("control-panel: {error}");
            ExitCode::FAILURE
        }
    }
}
