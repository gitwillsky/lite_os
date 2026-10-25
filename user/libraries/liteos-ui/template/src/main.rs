use liteos_ui::{
    Application, ControlFlow, EventKind, Layout, WidgetKind, WidgetSpec, WindowOptions,
};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = Application::new()?;
    let window = app.window(WindowOptions::new(
        "Rust 应用",
        "org.liteos.example",
        420,
        220,
    ))?;
    let mut spec = WidgetSpec::new(WidgetKind::Column, "");
    spec.layout = Layout {
        grow: 1,
        padding: 12,
        gap: 8,
        ..Layout::default()
    };
    let column = window.widget(spec)?;
    let label = column.child(WidgetSpec::new(WidgetKind::Label, "共享控件运行库已连接"))?;
    let button = column.child(WidgetSpec::new(WidgetKind::Button, "异步更新"))?;
    let mut task = None;
    app.run(|app, event| {
        if event.node == button.id() && matches!(event.kind, EventKind::Activate { .. }) {
            let label = label.clone();
            let delay = app.sleep(Duration::from_secs(1))?;
            task = Some(app.spawn_local(&window, async move {
                delay.await;
                let _ = label.set_text("异步等待完成，窗口始终响应输入");
            })?);
        }
        Ok(ControlFlow::Continue)
    })?;
    Ok(())
}
