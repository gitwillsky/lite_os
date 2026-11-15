//! 「工具」菜单的 Ping、路由跟踪与带宽测试窗口。每种工具是独立窗口，各自保留输入、结果与运行状态。
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

/// 诊断工具的种类，对应「工具」菜单的一项。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Tool {
    Ping,
    Trace,
    Bandwidth,
}

/// 一个启动按钮及其发起的探测；带宽测试的端口在启动时从表单读取。
#[derive(Clone, Copy)]
enum Start {
    Ping,
    Trace,
    Bandwidth { download: bool },
}

#[derive(Clone)]
struct Controls {
    target: Widget,
    port: Option<Widget>,
    starts: Vec<(Widget, Start)>,
    stop: Widget,
    output: Widget,
    status: Widget,
}

impl Controls {
    fn running(&self, running: bool) -> liteos_ui::Result<()> {
        let idle = if running { state::DISABLED } else { 0 };
        self.target.set_state(idle)?;
        if let Some(port) = &self.port {
            port.set_state(idle)?;
        }
        for (index, (widget, _)) in self.starts.iter().enumerate() {
            // 首个启动按钮是默认按钮：在目标地址框回车即启动。
            let default = if index == 0 { state::DEFAULT } else { 0 };
            widget.set_state(if running { state::DISABLED } else { default })?;
        }
        self.stop
            .set_state(if running { 0 } else { state::DISABLED })
    }
}

pub(super) struct Page {
    // 表单标签只需存活；控件句柄释放即删除节点。
    _captions: Vec<Widget>,
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
    pub(super) fn new(
        root: &Widget,
        background: Background,
        tool: Tool,
    ) -> liteos_ui::Result<Self> {
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
        let mut captions = Vec::new();
        let (caption, target) = field(
            &form,
            "目标地址：",
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        captions.push(caption);
        let port = if tool == Tool::Bandwidth {
            let (caption, port) = field(
                &form,
                "测速端口：",
                "5201",
                Layout {
                    width: 58,
                    ..Layout::default()
                },
            )?;
            captions.push(caption);
            Some(port)
        } else {
            None
        };
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
        let starts: &[(&str, Start)] = match tool {
            Tool::Ping => &[("开始", Start::Ping)],
            Tool::Trace => &[("开始", Start::Trace)],
            Tool::Bandwidth => &[
                ("测上传", Start::Bandwidth { download: false }),
                ("测下载", Start::Bandwidth { download: true }),
            ],
        };
        let starts = starts
            .iter()
            .map(|(text, start)| {
                Ok((
                    child(&commands, WidgetKind::Button, text, button())?,
                    *start,
                ))
            })
            .collect::<liteos_ui::Result<_>>()?;
        let controls = Controls {
            target,
            port,
            starts,
            stop: child(&commands, WidgetKind::Button, "停止", button())?,
            output,
            status: child(root, WidgetKind::Status, "", Layout::default())?,
        };
        controls
            .output
            .set_list_style(ListStyle::Details, ListOptions::default())?;
        let kind = match tool {
            Tool::Ping => Kind::Ping,
            Tool::Trace => Kind::Trace,
            Tool::Bandwidth => Kind::Bandwidth {
                port: NonZeroU16::MIN,
                download: false,
            },
        };
        let columns: Vec<_> = kind
            .columns()
            .iter()
            .enumerate()
            .map(|(index, (title, width))| Column {
                id: index as u64 + 1,
                title,
                width: *width,
            })
            .collect();
        controls.output.set_columns(&columns)?;
        controls.running(false)?;
        Ok(Self {
            _captions: captions,
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
        let start = if event.node == controls.target.id() {
            controls.starts.first().map(|(_, start)| *start)
        } else {
            controls
                .starts
                .iter()
                .find(|(widget, _)| widget.id() == event.node)
                .map(|(_, start)| *start)
        };
        let kind = match start {
            None => return Ok(()),
            Some(Start::Ping) => Kind::Ping,
            Some(Start::Trace) => Kind::Trace,
            Some(Start::Bandwidth { download }) => {
                let port = match &controls.port {
                    Some(port) => port.text()?.trim().parse::<NonZeroU16>().ok(),
                    None => None,
                };
                let Some(port) = port else {
                    return controls.status.set_text("端口请输入 1 到 65535。");
                };
                Kind::Bandwidth { port, download }
            }
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
