//! 「工具 → 网络诊断」窗口：Ping、逐跳探测与带宽测试。
//! 采用 WinBox 工具窗口布局：左侧为右对齐标签的表单和结果列表，右侧一列命令按钮。
use crate::{
    child,
    probe::{Cancel, Kind, Probe},
};
use liteos_ui::{
    Application, Background, Column, Event, EventKind, Layout, ListOptions, ListStyle, Task,
    Widget, WidgetKind, WidgetSpec, Window, state,
};
use std::{num::NonZeroU16, sync::Arc};

/// 表单标签列宽（逻辑像素）。
const LABEL: i32 = 84;

/// 右侧命令按钮的标准尺寸。
fn button() -> Layout {
    Layout {
        width: 82,
        height: 23,
        ..Layout::default()
    }
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
        let idle = if running { state::DISABLED } else { 0 };
        self.target.set_state(idle)?;
        self.ping.set_state(if running {
            state::DISABLED
        } else {
            state::DEFAULT
        })?;
        for widget in [&self.trace, &self.port, &self.upload, &self.download] {
            widget.set_state(idle)?;
        }
        self.stop
            .set_state(if running { 0 } else { state::DISABLED })
    }
}

pub(super) struct Page {
    // 表单标签只需存活；控件句柄释放即删除节点。
    _captions: [Widget; 2],
    controls: Controls,
    background: Background,
    active: Option<(Cancel, Task<()>)>,
}

/// 一行「右对齐标签 + 输入」；返回（标签, 输入框），两者都须由调用者持有。
fn field(
    parent: &Widget,
    label: &str,
    text: &str,
    input: Layout,
) -> liteos_ui::Result<(Widget, Widget)> {
    let row = child(
        parent,
        WidgetKind::Row,
        "",
        Layout {
            gap: 6,
            ..Layout::default()
        },
    )?;
    let mut caption = WidgetSpec::new(WidgetKind::Label, label);
    caption.state = state::ALIGN_RIGHT;
    caption.layout = Layout {
        width: LABEL,
        ..Layout::default()
    };
    let caption = row.child(caption)?;
    Ok((caption, child(&row, WidgetKind::Edit, text, input)?))
}

impl Page {
    pub(super) fn new(root: &Widget, background: Background) -> liteos_ui::Result<Self> {
        let body = child(
            root,
            WidgetKind::Row,
            "",
            Layout {
                grow: 1,
                gap: 8,
                ..Layout::default()
            },
        )?;
        let form = child(
            &body,
            WidgetKind::Column,
            "",
            Layout {
                grow: 1,
                gap: 4,
                ..Layout::default()
            },
        )?;
        let (target_caption, target) = field(
            &form,
            "目标地址：",
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        let (port_caption, port) = field(
            &form,
            "测速端口：",
            "5201",
            Layout {
                width: 58,
                ..Layout::default()
            },
        )?;
        let output = child(
            &form,
            WidgetKind::List,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        let commands = child(
            &body,
            WidgetKind::Column,
            "",
            Layout {
                gap: 4,
                ..Layout::default()
            },
        )?;
        let controls = Controls {
            target,
            port,
            output,
            ping: child(&commands, WidgetKind::Button, "Ping", button())?,
            trace: child(&commands, WidgetKind::Button, "逐跳探测", button())?,
            upload: child(&commands, WidgetKind::Button, "测上传", button())?,
            download: child(&commands, WidgetKind::Button, "测下载", button())?,
            stop: child(&commands, WidgetKind::Button, "停止", button())?,
            status: child(root, WidgetKind::Status, "", Layout::default())?,
        };
        controls
            .output
            .set_list_style(ListStyle::Details, ListOptions::default())?;
        controls.output.set_columns(&[Column {
            id: 1,
            title: "探测结果",
            width: 520,
        }])?;
        controls.running(false)?;
        Ok(Self {
            _captions: [target_caption, port_caption],
            controls,
            background,
            active: None,
        })
    }

    /// 关闭窗口或退出时取消正在运行的探测，已收到的结果保留。
    pub(super) fn stop(&self) -> liteos_ui::Result<()> {
        if let Some((cancel, _)) = &self.active {
            cancel.stop();
            self.controls.stop.set_state(state::DISABLED)?;
            self.controls.status.set_text("正在停止…")?;
        }
        Ok(())
    }

    /// 处理本窗口的按钮与回车；`domain` 是顶栏当前选中的路由域。
    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        event: &Event,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        if self
            .active
            .as_ref()
            .is_some_and(|(_, task)| task.is_finished())
        {
            self.active = None;
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        let controls = &self.controls;
        if self.active.is_some() {
            if event.node == controls.stop.id() {
                self.stop()?;
            }
            return Ok(());
        }
        let kind = if event.node == controls.ping.id() || event.node == controls.target.id() {
            Kind::Ping
        } else if event.node == controls.trace.id() {
            Kind::Trace
        } else if event.node == controls.upload.id() || event.node == controls.download.id() {
            let Ok(port) = controls.port.text()?.trim().parse::<NonZeroU16>() else {
                return controls.status.set_text("端口请输入 1 到 65535。");
            };
            Kind::Bandwidth {
                port,
                download: event.node == controls.download.id(),
            }
        } else {
            return Ok(());
        };
        match Probe::start(
            app,
            &self.background,
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
                let task = app.spawn_local(window, async move {
                    let text = probe
                        .display(&controls.output)
                        .await
                        .unwrap_or_else(|error| error.to_string());
                    let _ = controls.status.set_text(&format!("{scope}　{text}"));
                    let _ = controls.running(false);
                })?;
                self.active = Some((cancel, task));
            }
            Err(error) => controls.status.set_text(&error.to_string())?,
        }
        Ok(())
    }
}
