//! “拔出或弹出硬件”窗口；后台发现与移除，UI 只持有这次连接的标准 sysfs 句柄。
use linux_uapi::usb::{self, Device};
use liteos_ui::{
    Application, ControlFlow, EventKind, Layout, MessageButtons, MessageResult, Widget, WidgetKind,
    WidgetSpec, WindowOptions, state,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

/// 显示当前 USB 连接；停止动作完成前不宣称可以拔出。
/// # Errors
/// UI 或后台任务创建失败返回错误；设备错误在窗口内报告。
pub(super) fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = Application::new()?;
    // 原版是固定尺寸对话框：无最大化/最小化，由窗口管理器居中。
    let mut options = WindowOptions::new("拔出或弹出硬件", "org.liteos.device-manager", 360, 320);
    options.fixed = true;
    let window = app.window(options)?;
    let mut spec = WidgetSpec::new(WidgetKind::Column, "");
    spec.layout = Layout {
        grow: 1,
        padding: 10,
        gap: 6,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    let node = |parent: &Widget, kind, text: &str, layout| {
        let mut spec = WidgetSpec::new(kind, text);
        spec.layout = layout;
        parent.child(spec)
    };
    let intro = node(
        &root,
        WidgetKind::Label,
        "选择要拔出或弹出的硬件设备，然后单击“停止”。当 Windows 通知您可以安全地这样做时，请拔出设备。",
        Layout {
            height: 48,
            ..Default::default()
        },
    )?;
    intro.set_state(state::WRAP)?;
    let caption = node(
        &root,
        WidgetKind::Label,
        "硬件设备(H):",
        Layout::default(),
    )?;
    let list = node(
        &root,
        WidgetKind::List,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    let status = node(
        &root,
        WidgetKind::Label,
        "正在读取设备…",
        Layout {
            height: 32,
            ..Default::default()
        },
    )?;
    status.set_state(state::WRAP)?;
    let buttons = node(
        &root,
        WidgetKind::Row,
        "",
        Layout {
            height: 23,
            gap: 6,
            ..Default::default()
        },
    )?;
    let spacer = node(
        &buttons,
        WidgetKind::Label,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    let button = Layout {
        width: 75,
        ..Default::default()
    };
    let stop = node(&buttons, WidgetKind::Button, "停止(S)", button)?;
    let close = node(&buttons, WidgetKind::Button, "关闭(C)", button)?;
    // 句柄释放会移除控件，静态文字与容器必须与窗口同寿命。
    let _held = [&intro, &caption, &spacer, &buttons];
    stop.set_state(state::DISABLED)?;
    let devices = Rc::new(RefCell::new(Vec::<Device>::new()));
    let busy = Rc::new(Cell::new(false));
    let background = app.background()?;
    let _scan = {
        let background = background.clone();
        let devices = devices.clone();
        let busy = busy.clone();
        let list = list.clone();
        let stop = stop.clone();
        let status = status.clone();
        let clock = app.clock();
        app.spawn_local(&window, async move {
            loop {
                match background.run(usb::devices) {
                    Ok(work) => match work.await {
                        Ok(snapshot) => {
                            let selected = list.selection().ok().flatten();
                            if *devices.borrow() != snapshot {
                                let rows = snapshot
                                    .iter()
                                    .map(|d| (d.key.1, format!("{} ({})", d.name, d.location)))
                                    .collect::<Vec<_>>();
                                let _ = list.set_rows(
                                    &rows
                                        .iter()
                                        .map(|(id, text)| (*id, text.as_str()))
                                        .collect::<Vec<_>>(),
                                );
                                let selected = selected
                                    .filter(|id| snapshot.iter().any(|d| d.key.1 == *id))
                                    .or_else(|| snapshot.first().map(|d| d.key.1));
                                let _ = list.select(selected);
                                *devices.borrow_mut() = snapshot;
                            }
                            if !busy.get() {
                                let selected = list.selection().ok().flatten();
                                let enabled = devices
                                    .borrow()
                                    .iter()
                                    .any(|d| Some(d.key.1) == selected && d.can_remove());
                                let _ = stop.set_state(if enabled { 0 } else { state::DISABLED });
                            }
                            if status.text().ok().as_deref() == Some("正在读取设备…") {
                                let _ = status.set_text("停止设备可能会中断正在进行的播放。");
                            }
                        }
                        Err(error) => {
                            let _ = status.set_text(&format!("读取设备失败：{error}"));
                        }
                    },
                    Err(_) => break,
                }
                let Ok(wait) = clock.sleep(Duration::from_millis(500)) else {
                    break;
                };
                wait.await;
            }
        })?
    };
    let dialogs = app.dialogs();
    let mut removal = None;
    app.run(|app, event| {
        if event.window != window.id() {
            return Ok(ControlFlow::Continue);
        }
        if matches!(event.kind, EventKind::Close)
            || matches!(event.kind, EventKind::Activate { .. }) && event.node == close.id()
        {
            return Ok(ControlFlow::Exit);
        }
        if matches!(event.kind, EventKind::Select { .. }) && event.node == list.id() && !busy.get()
        {
            let selected = list.selection()?;
            let enabled = devices
                .borrow()
                .iter()
                .any(|d| Some(d.key.1) == selected && d.can_remove());
            stop.set_state(if enabled { 0 } else { state::DISABLED })?;
        }
        if matches!(event.kind, EventKind::Activate { .. })
            && event.node == stop.id()
            && !busy.replace(true)
        {
            let selected = list.selection()?;
            let device = devices
                .borrow()
                .iter()
                .find(|d| Some(d.key.1) == selected)
                .cloned();
            let Some(device) = device else {
                busy.set(false);
                return Ok(ControlFlow::Continue);
            };
            let window = window.clone();
            let status = status.clone();
            let stop = stop.clone();
            let dialogs = dialogs.clone();
            let busy = busy.clone();
            let background = background.clone();
            stop.set_state(state::DISABLED)?;
            removal = Some(app.spawn_local(&window.clone(), async move {
                let message = format!("确实要停止 {} 吗？正在使用它的程序将中断。", device.name);
                if let Ok(question) =
                    dialogs.message(&window, "停止硬件设备", &message, MessageButtons::YesNo)
                    && matches!(question.await, Ok(MessageResult::Yes))
                {
                    let _ = status.set_text("正在停止设备…");
                    match background.run(move || device.remove()) {
                        Ok(work) => match work.await {
                            Ok(()) => {
                                let _ = status.set_text("现在可以安全地拔出设备。");
                            }
                            Err(error) => {
                                let _ =
                                    status.set_text(&format!("无法停止设备，请勿拔出：{error}"));
                            }
                        },
                        Err(error) => {
                            let _ = status.set_text(&error.to_string());
                        }
                    }
                }
                busy.set(false);
            })?);
        }
        Ok(ControlFlow::Continue)
    })?;
    drop(removal);
    Ok(())
}
