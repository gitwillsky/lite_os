//! 音量控制只提交控件规格和 mixer 操作；布局、输入、绘制与帧调度由共享运行库负责。
extern crate liteos_runtime as _;
use liteos_ui::{
    Application, ControlFlow, EventKind, Layout, Menu, MenuItem, Range, Widget, WidgetKind as Kind,
    WidgetSpec, Window, WindowOptions, state,
};
use sndvol::Volume;
use std::{cell::RefCell, process::ExitCode, rc::Rc, time::Duration};

struct MixerControls {
    balance: Widget,
    volume: Widget,
    mute: Widget,
    message: Widget,
}
impl MixerControls {
    fn refresh(&self, model: &Volume) -> liteos_ui::Result<()> {
        let disabled = if model.value.is_none() {
            state::DISABLED
        } else {
            0
        };
        self.balance.set_state(disabled)?;
        self.volume.set_state(disabled)?;
        self.mute.set_state(
            disabled
                | if model.value.is_some_and(|v| v.muted) {
                    state::CHECKED
                } else {
                    0
                },
        )?;
        self.balance.set_range(Range {
            minimum: -100,
            maximum: 100,
            value: model.balance,
            page: 1,
            step: 5,
            vertical: 0,
        })?;
        self.volume.set_range(Range {
            minimum: 0,
            maximum: 100,
            value: 100 - model.value.map_or(0, |v| i32::from(v.left.max(v.right))),
            page: 1,
            step: 5,
            vertical: 1,
        })?;
        self.message.set_text(model.message)
    }
}
fn child(parent: &Widget, kind: Kind, text: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}
fn about(app: &mut Application, parent: &Window) -> liteos_ui::Result<(Window, Vec<Widget>)> {
    let mut options = WindowOptions::new("关于音量控制", "org.liteos.sndvol", 300, 160);
    options.fixed = true;
    options.parent = Some(parent);
    options.modal = true;
    let window = app.window(options)?;
    let mut root = WidgetSpec::new(Kind::Column, "");
    root.layout = Layout {
        grow: 1,
        padding: 12,
        gap: 10,
        ..Default::default()
    };
    let root = window.widget(root)?;
    let label = child(
        &root,
        Kind::Label,
        "LiteOS 音量控制\n调整主音量与左右平衡。",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    label.set_state(state::WRAP)?;
    let button = child(
        &root,
        Kind::Button,
        "确定",
        Layout {
            height: 25,
            ..Default::default()
        },
    )?;
    button.set_state(state::DEFAULT)?;
    Ok((window, vec![root, label, button]))
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = Application::new()?;
    let mut options = WindowOptions::new("音量控制", "org.liteos.sndvol", 180, 390);
    options.fixed = true;
    let window = app.window(options)?;
    let mut root_spec = WidgetSpec::new(Kind::Column, "");
    root_spec.layout.grow = 1;
    let root = window.widget(root_spec)?;
    let menu = child(&root, Kind::MenuBar, "", Layout::default())?;
    menu.set_menus(&[
        Menu {
            title: "选项(P)",
            items: &[MenuItem {
                id: 1,
                label: "退出(X)",
                shortcut: "",
                state: 0,
            }],
        },
        Menu {
            title: "帮助(H)",
            items: &[MenuItem {
                id: 2,
                label: "关于音量控制(A)",
                shortcut: "",
                state: 0,
            }],
        },
    ])?;
    let body = child(
        &root,
        Kind::Column,
        "",
        Layout {
            grow: 1,
            padding: 8,
            gap: 5,
            ..Default::default()
        },
    )?;
    let title = child(&body, Kind::Label, "音量控制", Layout::default())?;
    let balance_label = child(&body, Kind::Label, "平衡：", Layout::default())?;
    let balance = child(
        &body,
        Kind::Slider,
        "",
        Layout {
            height: 24,
            ..Default::default()
        },
    )?;
    let volume_label = child(&body, Kind::Label, "音量：", Layout::default())?;
    let volume = child(
        &body,
        Kind::Slider,
        "",
        Layout {
            grow: 1,
            min_height: 90,
            ..Default::default()
        },
    )?;
    let mute = child(&body, Kind::CheckBox, "全部静音(M)", Layout::default())?;
    let message = child(
        &body,
        Kind::Label,
        "",
        Layout {
            height: 40,
            ..Default::default()
        },
    )?;
    message.set_state(state::WRAP)?;
    let controls = Rc::new(MixerControls {
        balance,
        volume,
        mute,
        message,
    });
    let model = Rc::new(RefCell::new(Volume::open()));
    controls.refresh(&model.borrow())?;
    let clock = app.clock();
    let timer_model = model.clone();
    let timer_controls = controls.clone();
    // 采样只服务 mixer 外部变化；定时 Future 等待真实 deadline，不按输入次数放大 ioctl。
    let _refresh = app.spawn_local(&window, async move {
        loop {
            match clock.sleep(Duration::from_millis(100)) {
                Ok(wait) => wait.await,
                Err(error) => {
                    eprintln!("sndvol: {error}");
                    break;
                }
            }
            let mut model = timer_model.borrow_mut();
            if model.poll()
                && let Err(error) = timer_controls.refresh(&model)
            {
                eprintln!("sndvol: {error}");
                break;
            }
        }
    })?;
    let mut about_window: Option<(Window, Vec<Widget>)> = None;
    // 保留非交互控件 owner；最后一个句柄析构才会删除对应节点。
    let _labels = [title, balance_label, volume_label];
    app.run(|app, event| {
        if event.window != window.id() {
            if about_window
                .as_ref()
                .is_some_and(|(w, _)| w.id() == event.window)
                && matches!(
                    event.kind,
                    EventKind::Activate { .. }
                        | EventKind::Close
                        | EventKind::Key { pressed: true, .. }
                )
                && let Some((w, _)) = about_window.take()
            {
                w.close()?;
            }
            return Ok(ControlFlow::Continue);
        }
        match event.kind {
            EventKind::Activate { value: 1 } if event.node == menu.id() => {
                return Ok(ControlFlow::Exit);
            }
            EventKind::Activate { value: 2 } if event.node == menu.id() => {
                if let Some((w, _)) = about_window.take() {
                    w.close()?;
                }
                about_window = Some(about(app, &window)?);
            }
            EventKind::Key {
                code: 1,
                pressed: true,
                handled: false,
                ..
            } => return Ok(ControlFlow::Exit),
            EventKind::Changed { value } => {
                let mut model = model.borrow_mut();
                if event.node == controls.balance.id() {
                    model.set_balance(value as i32);
                } else if event.node == controls.volume.id()
                    && let Some(v) = model.value
                {
                    model.change((100 - value).clamp(0, 100) as u8, v.muted);
                }
                controls.refresh(&model)?;
            }
            EventKind::Activate { value } if event.node == controls.mute.id() => {
                let mut model = model.borrow_mut();
                if let Some(v) = model.value {
                    model.change(v.left.max(v.right), value != 0);
                }
                controls.refresh(&model)?;
            }
            _ => {}
        }
        Ok(ControlFlow::Continue)
    })?;
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sndvol: error: {error}");
            ExitCode::FAILURE
        }
    }
}
