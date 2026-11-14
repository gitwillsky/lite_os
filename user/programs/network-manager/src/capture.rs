//! 接口流量页的 PCAP 保存操作。

use crate::child;
use liteos_ui::{
    Application, Background, Dialogs, FileDialogMode, FileFilter, Layout, Task, Widget, WidgetKind,
    Window, state,
};
use network_config::{Capture, CaptureEnd, CaptureStop};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
};

pub(super) struct Controls {
    view: Rc<View>,
    task: Option<Task<()>>,
}

struct View {
    start: Widget,
    stop: Widget,
    status: Widget,
    dialogs: Dialogs,
    background: Background,
    cancellation: RefCell<Option<CaptureStop>>,
    close_when_saved: Cell<bool>,
}

impl Controls {
    pub(super) fn new(
        bar: &Widget,
        parent: &Widget,
        dialogs: Dialogs,
        background: Background,
        button: Layout,
    ) -> liteos_ui::Result<Self> {
        let start = child(bar, WidgetKind::Button, "抓包…", button)?;
        let stop = child(bar, WidgetKind::Button, "停止抓包", button)?;
        stop.set_state(state::DISABLED)?;
        let status = child(parent, WidgetKind::Label, "", Layout::default())?;
        status.set_state(state::WRAP)?;
        Ok(Self {
            view: Rc::new(View {
                start,
                stop,
                status,
                dialogs,
                background,
                cancellation: RefCell::default(),
                close_when_saved: Cell::new(false),
            }),
            task: None,
        })
    }

    pub(super) fn finish_before_close(&self) -> bool {
        let cancellation = self.view.cancellation.borrow();
        let Some(stop) = cancellation.as_ref() else {
            return false;
        };
        // 保留窗口直到后台收完记录并同步文件。重复请求不提前终止正在写入的文件。
        self.view.close_when_saved.set(true);
        let _ = stop.stop();
        true
    }

    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        node: u64,
        selected: Option<u64>,
    ) -> liteos_ui::Result<bool> {
        if node == self.view.stop.id() {
            if let Some(stop) = self.view.cancellation.borrow().as_ref() {
                let _ = stop.stop();
            }
            self.view.stop.set_state(state::DISABLED)?;
            self.view.status.set_text("正在结束抓包…")?;
            return Ok(true);
        }
        if node != self.view.start.id() {
            return Ok(false);
        }
        if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(true);
        }
        let Some(index) = selected.and_then(|id| u32::try_from(id).ok()) else {
            self.view.status.set_text("请先选择要抓包的接口。")?;
            return Ok(true);
        };
        let parent = window.clone();
        let view = self.view.clone();
        view.start.set_state(state::DISABLED)?;
        self.task = Some(app.spawn_local(window, async move {
            let result = view.save(&parent, index).await;
            view.cancellation.borrow_mut().take();
            let _ = view.stop.set_state(state::DISABLED);
            let _ = view.start.set_state(0);
            let status = result.unwrap_or_else(|error| format!("抓包失败。{error}"));
            let _ = view.status.set_text(&status);
            if view.close_when_saved.get() {
                let _ = parent.close();
            }
        })?);
        Ok(true)
    }
}

impl View {
    async fn save(
        &self,
        parent: &Window,
        index: u32,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let directory = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let Some(path) = self
            .dialogs
            .file_filtered(
                parent,
                FileDialogMode::SaveNew,
                &directory,
                "capture.pcap",
                FileFilter {
                    label: "报文捕获文件 (*.pcap)",
                    extensions: &["pcap"],
                },
            )?
            .await?
        else {
            return Ok("已取消抓包".into());
        };
        self.status.set_text("正在启动抓包…")?;
        let (capture, stop) = self.background.run(move || Capture::start(index))?.await?;
        *self.cancellation.borrow_mut() = Some(stop);
        self.stop.set_state(0)?;
        self.status.set_text("正在抓包，最多 60 秒。")?;
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        let result = self
            .background
            .run(move || {
                linux_uapi::filesystem::write_private_new(&path, |file| capture.write(file))
            })?
            .await?;
        let reason = match result.reason {
            CaptureEnd::Stopped => "已停止",
            CaptureEnd::TimeLimit => "已达到时限",
            CaptureEnd::SizeLimit => "已达到文件大小上限",
            CaptureEnd::InterfaceDown => "接口已不可用",
        };
        Ok(format!(
            "{reason}，已保存 {name}。{} 个报文，抓包缓冲丢弃 {} 个。",
            result.packets, result.drops
        ))
    }
}
