//! 配置备份页。列表只保留备份身份，实际内容和调度状态由系统配置服务提供。

use crate::{child, configuration::review_and_save, json_dialog};
use liteos_ui::{
    Application, Background, Column, Dialogs, Event, EventKind, FileDialogMode, Layout,
    ListOptions, ListStyle, MessageButtons, MessageIcon, MessageResult, Task, Widget, WidgetKind,
    Window, state,
};
use network_config::{
    ManagementClient,
    configuration::{
        BackupActivity, BackupId, CommitResult, RouterConfiguration, new_commit_id, save_new,
    },
    format_lease_time,
};
use std::{cell::RefCell, io, rc::Rc};

mod schedule;

type Error = Box<dyn std::error::Error>;

enum Action {
    Refresh,
    Create,
    Configure,
    Export(BackupId),
    Delete(BackupId),
    Restore(BackupId),
}

pub(super) struct Page {
    view: Rc<View>,
    task: Option<Task<()>>,
}

struct View {
    dialogs: Dialogs,
    background: Background,
    list: Widget,
    refresh: Widget,
    create: Widget,
    export: Widget,
    delete: Widget,
    restore: Widget,
    configure: Widget,
    schedule: Widget,
    status: Widget,
    entries: RefCell<Vec<BackupId>>,
}

impl Page {
    pub(super) fn new(
        parent: &Widget,
        dialogs: Dialogs,
        background: Background,
        button: Layout,
    ) -> liteos_ui::Result<Self> {
        let schedule = child(parent, WidgetKind::Label, "", Layout::default())?;
        let list = child(
            parent,
            WidgetKind::List,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        list.set_list_style(
            ListStyle::Details,
            ListOptions {
                multiple: false,
                full_row_select: true,
            },
        )?;
        list.set_columns(&[
            Column {
                id: 1,
                title: "时间",
                width: 192,
            },
            Column {
                id: 2,
                title: "类型",
                width: 68,
            },
            Column {
                id: 3,
                title: "大小",
                width: 102,
            },
            Column {
                id: 4,
                title: "备份编号",
                width: 246,
            },
        ])?;
        let bar = child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                gap: 6,
                ..Layout::default()
            },
        )?;
        let view = View {
            dialogs,
            background,
            list,
            refresh: child(&bar, WidgetKind::Button, "刷新", button)?,
            create: child(&bar, WidgetKind::Button, "创建备份", button)?,
            export: child(&bar, WidgetKind::Button, "导出…", button)?,
            delete: child(&bar, WidgetKind::Button, "删除…", button)?,
            restore: child(&bar, WidgetKind::Button, "恢复…", button)?,
            configure: child(&bar, WidgetKind::Button, "定时设置…", button)?,
            schedule,
            status: child(parent, WidgetKind::Status, "", Layout::default())?,
            entries: RefCell::default(),
        };
        view.running(false)?;
        Ok(Self {
            view: Rc::new(view),
            task: None,
        })
    }

    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        event: &Event,
        opened: bool,
    ) -> liteos_ui::Result<()> {
        if let Some(task) = &self.task {
            if !task.is_finished() {
                return Ok(());
            }
            self.task = None;
        }
        let view = &self.view;
        let action = if opened {
            Action::Refresh
        } else if event.node == view.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            return view.running(false);
        } else if matches!(event.kind, EventKind::Activate { .. }) {
            if event.node == view.refresh.id() {
                Action::Refresh
            } else if event.node == view.create.id() {
                Action::Create
            } else if event.node == view.configure.id() {
                Action::Configure
            } else if let Some(id) = view.selected()? {
                if event.node == view.export.id() {
                    Action::Export(id)
                } else if event.node == view.delete.id() {
                    Action::Delete(id)
                } else if event.node == view.restore.id() {
                    Action::Restore(id)
                } else {
                    return Ok(());
                }
            } else {
                return Ok(());
            }
        } else {
            return Ok(());
        };
        view.running(true)?;
        let view = view.clone();
        let parent = window.clone();
        self.task = Some(app.spawn_local(window, async move {
            let text = view
                .perform(&parent, action)
                .await
                .unwrap_or_else(|error| error.to_string());
            let _ = view.status.set_text(&text);
            let _ = view.running(false);
        })?);
        Ok(())
    }
}

impl View {
    fn selected(&self) -> liteos_ui::Result<Option<BackupId>> {
        let index = self
            .list
            .selection()?
            .and_then(|row| usize::try_from(row - 1).ok());
        Ok(index.and_then(|index| self.entries.borrow().get(index).copied()))
    }

    fn running(&self, running: bool) -> liteos_ui::Result<()> {
        let base = if running { state::DISABLED } else { 0 };
        self.refresh.set_state(base)?;
        self.create.set_state(base)?;
        self.configure.set_state(base)?;
        let selection = if running || self.selected()?.is_none() {
            state::DISABLED
        } else {
            0
        };
        for button in [&self.export, &self.delete, &self.restore] {
            button.set_state(selection)?;
        }
        Ok(())
    }

    async fn refresh(&self, preferred: Option<BackupId>) -> Result<usize, Error> {
        let result = self
            .background
            .run(|| ManagementClient::local().backups())?
            .await;
        let listing = match result {
            Ok(listing) => listing,
            Err(error) => {
                self.schedule.set_text("自动备份状态无法更新")?;
                return Err(error.into());
            }
        };
        let selected = preferred.or(self.selected()?);
        let mut entries = Vec::with_capacity(listing.entries.len());
        let mut rows = Vec::with_capacity(listing.entries.len());
        for entry in listing.entries {
            entries.push(entry.id);
            let time = format_lease_time(entry.modified_seconds);
            let kind = if entry.automatic { "自动" } else { "手动" };
            let size = entry.size;
            let id = entry.id;
            rows.push((
                entries.len() as u64,
                format!("{time}\t{kind}\t{size} 字节\t{id}"),
            ));
        }
        let displayed: Vec<_> = rows
            .iter()
            .map(|(row, text)| (*row, text.as_str()))
            .collect();
        self.list.set_rows(&displayed)?;
        let selection = selected.and_then(|id| entries.iter().position(|entry| *entry == id));
        *self.entries.borrow_mut() = entries;
        self.list.select(selection.map(|index| index as u64 + 1))?;

        let status = match listing.automatic.activity {
            BackupActivity::Disabled => "已停用",
            BackupActivity::Waiting => "等待中",
            BackupActivity::Working => "正在执行",
            BackupActivity::Retrying => "等待重试",
        };
        let mut schedule = format!("自动备份：{status}");
        if let Some(seconds) = listing.automatic.next_in_seconds {
            schedule.push_str(&format!("，下次运行将在 {seconds} 秒后"));
        }
        if let Some(error) = listing.automatic.error {
            schedule.push_str(&format!("，上次错误码 {error}"));
        }
        self.schedule.set_text(&schedule)?;
        Ok(rows.len())
    }

    async fn perform(&self, parent: &Window, action: Action) -> Result<String, Error> {
        match action {
            Action::Refresh => {
                self.status.set_text("正在读取备份…")?;
                let count = self.refresh(None).await?;
                Ok(if count == 0 {
                    "没有配置备份".into()
                } else {
                    format!("共 {count} 份配置备份")
                })
            }
            Action::Create => {
                self.status.set_text("正在创建备份…")?;
                let result = self
                    .background
                    .run(|| ManagementClient::local().create_backup())?
                    .await;
                let preferred = result.as_ref().ok().copied();
                let message = match result {
                    Ok(id) => format!("已创建备份 {id}"),
                    Err(error) => format!("创建结果未确认：{error}。请核对备份列表。"),
                };
                Ok(self.refresh_after_change(message, preferred).await)
            }
            Action::Export(id) => self.export(parent, id).await,
            Action::Configure => self.configure_schedule(parent).await,
            Action::Delete(id) => {
                if !self
                    .confirm(parent, "删除配置备份", &format!("确定删除备份 {id}？"))
                    .await?
                {
                    return Ok("已取消删除".into());
                }
                self.status.set_text("正在删除备份…")?;
                let result = self
                    .background
                    .run(move || ManagementClient::local().delete_backup(id))?
                    .await;
                let message = match result {
                    Ok(()) => format!("已删除备份 {id}"),
                    Err(error) => format!("删除结果未确认：{error}。请核对备份列表。"),
                };
                Ok(self.refresh_after_change(message, None).await)
            }
            Action::Restore(id) => self.restore(parent, id).await,
        }
    }

    async fn configure_schedule(&self, parent: &Window) -> Result<String, Error> {
        self.status.set_text("正在读取定时备份设置…")?;
        let (revision, mut config) = self
            .background
            .run(|| {
                let (revision, bytes) = ManagementClient::local().export()?;
                let config = RouterConfiguration::decode(&bytes).map_err(io::Error::other)?;
                Ok::<_, io::Error>((revision, config))
            })?
            .await?;
        let Some(policy) = schedule::edit(&self.dialogs, parent, config.backup).await? else {
            return Ok("已取消定时设置".into());
        };
        if policy == config.backup {
            return Ok("定时备份设置没有改动".into());
        }
        config.backup = policy;
        let candidate = self
            .background
            .run(move || config.encode().map_err(io::Error::other))?
            .await?;
        let result = review_and_save(
            &self.dialogs,
            parent,
            &self.background,
            &self.status,
            candidate,
            Some(revision),
        )
        .await?;
        Ok(self.refresh_after_change(result.message, None).await)
    }

    async fn refresh_after_change(&self, message: String, preferred: Option<BackupId>) -> String {
        match self.refresh(preferred).await {
            Ok(_) => message,
            Err(error) => format!("{message}。无法刷新列表：{error}"),
        }
    }

    async fn export(&self, parent: &Window, id: BackupId) -> Result<String, Error> {
        let Some(path) = json_dialog(
            &self.dialogs,
            parent,
            FileDialogMode::SaveNew,
            &format!("router-{id}.json"),
        )?
        .await?
        else {
            return Ok("已取消导出".into());
        };
        self.status.set_text("正在导出备份…")?;
        self.background
            .run(move || {
                let document = ManagementClient::local().export_backup(id)?;
                save_new(&path, &document)
            })?
            .await?;
        Ok(format!("已导出备份 {id}"))
    }

    async fn restore(&self, parent: &Window, id: BackupId) -> Result<String, Error> {
        self.status.set_text("正在读取当前配置…")?;
        let revision = self
            .background
            .run(|| {
                ManagementClient::local()
                    .snapshot()
                    .map(|snapshot| snapshot.generation)
            })?
            .await?;
        let message = format!("恢复备份 {id} 将替换当前路由设置，可能影响网络连接。确定恢复？");
        if !self.confirm(parent, "恢复路由设置", &message).await? {
            return Ok("已取消恢复".into());
        }
        let commit = self.background.run(new_commit_id)?.await?;
        self.status
            .set_text(&format!("正在恢复，提交编号 {commit}"))?;
        // 使用确认前的代际。其他管理员在确认期间修改配置时，由服务拒绝旧代际。
        let message = self
            .background
            .run(
                move || match ManagementClient::local().restore_backup(revision, id, commit) {
                    Ok(revision) => format!("已恢复路由设置，代际 {revision}"),
                    Err(error) => match ManagementClient::local().commit_result(commit) {
                        Ok(CommitResult::Saved) => {
                            format!("提交 {commit} 已保存。请核对应用状态。")
                        }
                        _ => format!("提交 {commit} 未确认，请核对配置。{error}"),
                    },
                },
            )?
            .await;
        Ok(message)
    }

    async fn confirm(
        &self,
        parent: &Window,
        title: &str,
        message: &str,
    ) -> liteos_ui::Result<bool> {
        Ok(self
            .dialogs
            .message(
                parent,
                title,
                message,
                MessageButtons::YesNo,
                MessageIcon::Warning,
            )?
            .await?
            == MessageResult::Yes)
    }
}
