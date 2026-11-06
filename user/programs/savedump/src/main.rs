//! 上次内核崩溃的恢复提示（Windows 2000 `savedump.exe`）。
use liteos_ui::{
    Application, ControlFlow, EventKind, Layout, WidgetKind as Kind, WidgetSpec, WindowOptions,
    state,
};
use std::{fs::OpenOptions, io::ErrorKind, process::ExitCode};

const APP_ID: &str = "org.liteos.savedump";
/// 本次启动已提示的标记；`/tmp` 是每次启动重建的 tmpfs，标记不会跨启动残留。
const SHOWN_MARKER: &str = "/tmp/.liteos-savedump";

/// 上次崩溃记录的摘要（首个非空行）；上次正常结束或已在本次启动提示过时为 `None`。
fn pending_record() -> std::io::Result<Option<String>> {
    let record = std::fs::read_to_string("/proc/last_panic")?;
    let Some(summary) = record.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return Ok(None);
    };
    // 原子创建标记：并发会话中只有第一个创建者提示。
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(SHOWN_MARKER)
    {
        Ok(_) => Ok(Some(summary.to_owned())),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(None),
        Err(error) => Err(error),
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Some(summary) = pending_record()? else {
        return Ok(());
    };
    let mut app = Application::new()?;
    let mut options = WindowOptions::new("系统错误", APP_ID, 420, 170);
    options.fixed = true;
    let window = app.window(options)?;
    let mut spec = WidgetSpec::new(Kind::Column, "");
    spec.layout = Layout {
        grow: 1,
        padding: 12,
        gap: 12,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    let text =
        format!("系统已从严重错误中恢复。\n\n错误：{summary}\n完整记录见 /proc/last_panic。");
    let mut message = WidgetSpec::new(Kind::Label, &text);
    message.layout.grow = 1;
    message.state = state::WRAP;
    let _message = root.child(message)?;
    let mut row = WidgetSpec::new(Kind::Row, "");
    row.layout.height = 26;
    let row = root.child(row)?;
    let mut spacer = WidgetSpec::new(Kind::Label, "");
    spacer.layout.grow = 1;
    let _spacer = row.child(spacer)?;
    let mut ok = WidgetSpec::new(Kind::Button, "确定");
    ok.layout.width = 80;
    ok.layout.height = 26;
    ok.state = state::DEFAULT;
    let ok = row.child(ok)?;
    app.run(|_, event| {
        let close = matches!(event.kind, EventKind::Close)
            || matches!(event.kind, EventKind::Activate { .. }) && event.node == ok.id();
        Ok(if close {
            ControlFlow::Exit
        } else {
            ControlFlow::Continue
        })
    })?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("savedump: {error}");
            ExitCode::FAILURE
        }
    }
}
