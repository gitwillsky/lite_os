//! 编辑草稿、审阅与整份配置提交；只有配置服务决定已保存的配置和生效状态。

use crate::{child, json_dialog};
use liteos_ui::{
    Application, Background, Dialogs, Event, EventKind, FileDialogMode, Layout, MAX_TEXT_BYTES,
    MessageButtons, MessageIcon, MessageResult, Task, Widget, WidgetKind, Window, state,
};
use network_config::{
    ConfigurationStatus, ManagementClient, Revision,
    configuration::{CommitResult, new_commit_id, save_new},
};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    fs::{self, File},
    io::{self, Read},
    path::Path,
    rc::Rc,
    sync::Arc,
};

mod review;

type Error = Box<dyn std::error::Error>;

enum Draft {
    Text(String),
    Document(Arc<Vec<u8>>),
}

impl Draft {
    fn from_file(path: &Path) -> io::Result<Self> {
        if !fs::metadata(path)?.is_file() {
            return Err(io::Error::other("请选择普通配置文件。"));
        }
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::other("请选择普通配置文件。"));
        }
        let length = usize::try_from(metadata.len()).map_err(io::Error::other)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| io::ErrorKind::OutOfMemory)?;
        file.by_ref().take(metadata.len()).read_to_end(&mut bytes)?;
        if bytes.len() != length || file.read(&mut [0])? != 0 {
            return Err(io::Error::other(
                "配置文件在读取过程中改变了大小，请重新导入。",
            ));
        }
        if bytes.len() > MAX_TEXT_BYTES {
            return Ok(Self::Document(Arc::new(bytes)));
        }
        let bytes = match String::from_utf8(bytes) {
            Ok(text) if !text.contains('\0') => return Ok(Self::Text(text)),
            Ok(text) => text.into_bytes(),
            Err(error) => error.into_bytes(),
        };
        let (_, document) = ManagementClient::local().validate(&bytes)?;
        Self::from_export(document)
    }

    fn from_export(document: Vec<u8>) -> io::Result<Self> {
        if document.len() > MAX_TEXT_BYTES {
            return Ok(Self::Document(Arc::new(document)));
        }
        let value: Value = serde_json::from_slice(&document)?;
        let formatted = serde_json::to_string_pretty(&value)?;
        if formatted.len() <= MAX_TEXT_BYTES {
            Ok(Self::Text(formatted))
        } else {
            String::from_utf8(document)
                .map(Self::Text)
                .map_err(io::Error::other)
        }
    }
}

impl AsRef<[u8]> for Draft {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Document(bytes) => bytes,
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    Read,
    Import,
    Export,
    Schema,
    Review,
    Status,
    Close,
}

pub(super) struct Page {
    view: Rc<View>,
    task: Option<Task<()>>,
}

struct View {
    dialogs: Dialogs,
    background: Background,
    buttons: Vec<(Widget, Action)>,
    draft: Widget,
    status: Widget,
    application: Widget,
    // 编辑器持有可编辑文本；超出编辑器容量的文档由视图持有，审阅和提交共享同一份只读字节。
    document: RefCell<Option<Arc<Vec<u8>>>>,
    opened: Cell<bool>,
    dirty: Cell<bool>,
}

impl Page {
    pub(super) fn new(
        parent: &Widget,
        dialogs: Dialogs,
        background: Background,
        button: Layout,
    ) -> liteos_ui::Result<Self> {
        let bar = child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                gap: 6,
                ..Layout::default()
            },
        )?;
        let buttons = [
            ("读取当前", Action::Read),
            ("导入文件…", Action::Import),
            ("导出草稿…", Action::Export),
            ("字段说明…", Action::Schema),
            ("审阅…", Action::Review),
            ("应用状态", Action::Status),
        ]
        .into_iter()
        .map(|(title, action)| Ok((child(&bar, WidgetKind::Button, title, button)?, action)))
        .collect::<liteos_ui::Result<_>>()?;
        let draft = child(
            parent,
            WidgetKind::TextEditor,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        let view = View {
            dialogs,
            background,
            buttons,
            draft,
            status: child(parent, WidgetKind::Status, "", Layout::default())?,
            application: child(parent, WidgetKind::Label, "", Layout::default())?,
            document: RefCell::default(),
            opened: Cell::new(false),
            dirty: Cell::new(false),
        };
        Ok(Self {
            view: Rc::new(view),
            task: None,
        })
    }

    fn running(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
    }

    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        event: &Event,
        opened: bool,
    ) -> liteos_ui::Result<()> {
        if event.node == self.view.draft.id()
            && matches!(event.kind, EventKind::Changed { .. })
            && self.view.document.borrow().is_none()
        {
            if !self.view.dirty.replace(true) {
                self.view.status.set_text("草稿已修改")?;
            }
            return Ok(());
        }
        if self.running() {
            return Ok(());
        }
        if opened && !self.view.opened.replace(true) {
            return self.start(app, window, Action::Read);
        }
        if matches!(event.kind, EventKind::Activate { .. })
            && let Some((_, action)) = self
                .view
                .buttons
                .iter()
                .find(|(button, _)| button.id() == event.node)
        {
            self.start(app, window, *action)?;
        }
        Ok(())
    }

    pub(super) fn confirm_close(
        &mut self,
        app: &mut Application,
        window: &Window,
    ) -> liteos_ui::Result<bool> {
        if self.running() {
            return Ok(true);
        }
        if self.view.dirty.get() {
            self.start(app, window, Action::Close)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn start(
        &mut self,
        app: &mut Application,
        window: &Window,
        action: Action,
    ) -> liteos_ui::Result<()> {
        // 冻结同一份草稿直至文件操作或审阅结束；程序仍可载入全文。只读状态撤销在途粘贴。
        let view = self.view.clone();
        view.set_running(true)?;
        let parent = window.clone();
        self.task = Some(app.spawn_local(window, async move {
            match view.perform(&parent, action).await {
                Ok(Some(message)) => {
                    let _ = view.status.set_text(&message);
                }
                Ok(None) => {}
                Err(error) => {
                    let message = error.to_string();
                    let _ = view.status.set_text(&message);
                    if let Ok(dialog) = view.dialogs.message(
                        &parent,
                        "路由配置",
                        &message,
                        MessageButtons::Ok,
                        MessageIcon::Error,
                    ) {
                        let _ = dialog.await;
                    }
                }
            }
            let _ = view.set_running(false);
        })?);
        Ok(())
    }
}

impl View {
    fn set_running(&self, running: bool) -> liteos_ui::Result<()> {
        self.draft
            .set_state(if running || self.document.borrow().is_some() {
                state::READ_ONLY
            } else {
                0
            })?;
        for (button, _) in &self.buttons {
            button.set_state(if running { state::DISABLED } else { 0 })?;
        }
        Ok(())
    }

    fn draft(&self) -> liteos_ui::Result<Draft> {
        if let Some(document) = self.document.borrow().clone() {
            Ok(Draft::Document(document))
        } else {
            self.draft.text().map(Draft::Text)
        }
    }

    fn show_draft(&self, draft: Draft) -> liteos_ui::Result<()> {
        let document = match draft {
            Draft::Text(text) => {
                self.draft.set_text(&text)?;
                None
            }
            Draft::Document(document) => {
                self.draft.set_text(&format!(
                    "已载入 {:.1} MiB 的配置。选择「审阅」查看差异并提交。",
                    document.len() as f64 / (1024.0 * 1024.0)
                ))?;
                Some(document)
            }
        };
        *self.document.borrow_mut() = document;
        self.dirty.set(false);
        Ok(())
    }

    async fn discard_changes(&self, parent: &Window) -> liteos_ui::Result<bool> {
        if !self.dirty.get() {
            return Ok(true);
        }
        Ok(self
            .dialogs
            .message(
                parent,
                "路由配置",
                "草稿有尚未导出或提交的修改。是否放弃这些修改？",
                MessageButtons::YesNo,
                MessageIcon::Warning,
            )?
            .await?
            == MessageResult::Yes)
    }

    async fn perform(&self, parent: &Window, action: Action) -> Result<Option<String>, Error> {
        let message = match action {
            Action::Read => {
                if !self.discard_changes(parent).await? {
                    return Ok(None);
                }
                self.status.set_text("正在读取当前设置…")?;
                let result = self
                    .background
                    .run(|| {
                        let (_, document) = ManagementClient::local().export()?;
                        Draft::from_export(document)
                    })?
                    .await;
                self.show_draft(result?)?;
                "已将当前设置载入草稿"
            }
            Action::Import => {
                if !self.discard_changes(parent).await? {
                    return Ok(None);
                }
                let Some(path) =
                    json_dialog(&self.dialogs, parent, FileDialogMode::Open, "")?.await?
                else {
                    return Ok(None);
                };
                self.status.set_text("正在打开配置文件…")?;
                let draft = self
                    .background
                    .run(move || Draft::from_file(&path))?
                    .await?;
                self.show_draft(draft)?;
                "已将文件载入草稿"
            }
            Action::Export | Action::Schema => {
                let (name, draft) = if matches!(action, Action::Schema) {
                    ("router-schema.json", None)
                } else {
                    ("router.json", Some(self.draft()?))
                };
                let Some(path) =
                    json_dialog(&self.dialogs, parent, FileDialogMode::SaveNew, name)?.await?
                else {
                    return Ok(None);
                };
                self.status.set_text("正在导出…")?;
                let is_draft = draft.is_some();
                self.background
                    .run(move || {
                        if let Some(draft) = draft {
                            save_new(&path, draft.as_ref())
                        } else {
                            save_new(&path, &ManagementClient::local().schema()?)
                        }
                    })?
                    .await?;
                if is_draft {
                    self.dirty.set(false);
                    "已导出草稿"
                } else {
                    "已导出配置字段说明"
                }
            }
            Action::Review => return self.review_and_submit(parent).await.map(Some),
            Action::Status => {
                self.refresh_status().await?;
                "已刷新应用状态"
            }
            Action::Close => {
                if self.discard_changes(parent).await? {
                    parent.close()?;
                }
                return Ok(None);
            }
        };
        Ok(Some(message.into()))
    }

    async fn refresh_status(&self) -> Result<(), Error> {
        self.application.set_text("正在查询应用状态…")?;
        let result = self
            .background
            .run(|| ManagementClient::local().snapshot())?
            .await;
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.application.set_text("无法查询应用状态")?;
                return Err(error.into());
            }
        };
        let status = match snapshot.configuration_status {
            ConfigurationStatus::Ready => "已生效",
            ConfigurationStatus::Applying => "正在应用",
            ConfigurationStatus::RollingBack => "正在恢复原配置",
            ConfigurationStatus::Rejected => "无法加载已保存的配置",
        };
        self.application
            .set_text(&format!("当前设置{status}（代际 {}）", snapshot.generation))?;
        Ok(())
    }

    async fn review_and_submit(&self, parent: &Window) -> Result<String, Error> {
        let result = review_and_save(
            &self.dialogs,
            parent,
            &self.background,
            &self.status,
            self.draft()?,
            None,
        )
        .await?;
        if result.saved {
            self.dirty.set(false);
        }
        let _ = self.refresh_status().await;
        Ok(result.message)
    }
}

/// 同一审阅窗口与原生配置事务对草稿和图形表单返回的结果。
pub(super) struct ReviewResult {
    pub(super) saved: bool,
    pub(super) message: String,
}

/// 冻结审阅和提交的同一正文。表单指定打开时的基准代际，文本草稿采用审阅时的代际。
/// # Errors
/// 当前配置已变化、候选无效、管理服务无法读取，或审阅界面无法打开。
pub(super) async fn review_and_save(
    dialogs: &Dialogs,
    parent: &Window,
    background: &Background,
    status: &Widget,
    candidate: impl AsRef<[u8]> + Send + 'static,
    expected: Option<Revision>,
) -> Result<ReviewResult, Error> {
    status.set_text("正在校验并比较配置…")?;
    let (candidate, revision, comparison, recovery) = background
        .run(move || {
            let mut client = ManagementClient::local();
            let (revision, document) = client.difference(candidate.as_ref())?;
            let snapshot = client.snapshot()?;
            if expected.is_some_and(|base| base != revision) || snapshot.generation != revision {
                return Err(io::Error::other("配置已经变化，请重新读取后审阅。"));
            }
            let recovery = snapshot.configuration_status == ConfigurationStatus::Rejected;
            Ok((
                candidate,
                revision,
                Arc::new(review::Review::new(document)?),
                recovery,
            ))
        })?
        .await?;
    if comparison.is_empty() {
        return Ok(ReviewResult {
            saved: false,
            message: "配置没有改动".into(),
        });
    }
    if !review::confirm(dialogs, parent, background, comparison, recovery).await? {
        return Ok(ReviewResult {
            saved: false,
            message: "已取消提交".into(),
        });
    }
    let commit = background.run(new_commit_id)?.await?;
    status.set_text(&format!("正在保存，提交编号 {commit}"))?;
    let result = background
        .run(move || {
            let mut client = ManagementClient::local();
            match client.import(revision, commit, candidate.as_ref()) {
                Ok(_) => Ok(()),
                Err(error) => match client.commit_result(commit) {
                    Ok(CommitResult::Saved) => Ok(()),
                    _ => Err(error),
                },
            }
        })?
        .await;
    let saved = result.is_ok();
    let message = match result {
        Ok(()) => format!("提交 {commit} 已保存"),
        Err(error) => format!("提交 {commit} 未确认。{error}。请核对当前配置。"),
    };
    Ok(ReviewResult { saved, message })
}
