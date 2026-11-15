//! IP 和 IPv6 设置。各窗口绑定打开时选中的域，配置服务拥有持久化和运行状态。

use crate::{
    child,
    configuration::{DraftBaseline, apply},
    item_properties::{Footer, Request},
    workspace::Workspace,
};
use liteos_ui::{
    Application, Background, Event, Layout, MessageButtons, MessageIcon, Task, Widget, WidgetKind,
    Window, state,
};
use network_config::{
    ManagementClient, Revision,
    configuration::{RouterConfiguration, RoutingFamily as Family},
};
use std::{cell::RefCell, io, rc::Rc, sync::Arc};

pub(super) struct Settings {
    family: Family,
    background: Background,
    form: Rc<RefCell<Option<Form>>>,
    reader: Option<Task<()>>,
    writer: Option<Task<()>>,
}

struct Form {
    frame: Widget,
    forward: Widget,
    buttons: Footer,
    _body: [Widget; 2],
    domain: Option<Arc<str>>,
    draft: Option<Draft>,
}

struct Draft {
    baseline: DraftBaseline,
    forward: bool,
}

impl Settings {
    /// 创建一个地址族的设置窗口 owner；首次打开才访问配置服务。
    pub(super) fn new(background: Background, family: Family) -> Self {
        Self {
            family,
            background,
            form: Rc::default(),
            reader: None,
            writer: None,
        }
    }

    /// 打开或置前所选域的设置窗口。
    /// 错误：工作区已关闭、窗口创建或读取任务启动失败。
    pub(super) fn open(
        &mut self,
        app: &mut Application,
        window: &Window,
        workspace: &Workspace,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        if let Some(form) = &*self.form.borrow() {
            return workspace.raise(&form.frame);
        }
        let family = self.family;
        let frame = workspace.item_window(title(family), (366, 152))?;
        let root = child(
            &frame,
            WidgetKind::Row,
            "",
            Layout {
                grow: 1,
                padding: 8,
                gap: 8,
                ..Layout::default()
            },
        )?;
        let fields = child(
            &root,
            WidgetKind::Column,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        let forward = child(
            &fields,
            WidgetKind::CheckBox,
            match family {
                Family::Ipv4 => "IP 转发",
                Family::Ipv6 => "转发",
            },
            Layout::default(),
        )?;
        let buttons = Footer::side(&root)?;
        buttons.set_enabled(true, false)?;
        forward.set_state(state::DISABLED)?;
        let id = frame.id();
        let query_domain = domain.clone();
        *self.form.borrow_mut() = Some(Form {
            frame,
            forward,
            buttons,
            _body: [root, fields],
            domain,
            draft: None,
        });
        let form = self.form.clone();
        let background = self.background.clone();
        let dialogs = app.dialogs();
        let parent = window.clone();
        self.reader = Some(app.spawn_local(window, async move {
            let result = async {
                let draft = background
                    .run(move || Draft::load(query_domain.as_deref(), family))?
                    .await?;
                if let Some(form) = matching(&mut form.borrow_mut(), id) {
                    form.publish(draft)?;
                }
                Ok::<_, Box<dyn std::error::Error>>(())
            }
            .await;
            if let Err(error) = result
                && matching(&mut form.borrow_mut(), id).is_some()
                && let Ok(message) = dialogs.message(
                    &parent,
                    title(family),
                    &error.to_string(),
                    MessageButtons::Ok,
                    MessageIcon::Error,
                )
            {
                let _ = message.await;
            }
        })?);
        Ok(())
    }

    /// 切换域时关闭原窗口，未应用的输入随窗口释放。在途提交仍由原域的事务完成。
    pub(super) fn close(&mut self) {
        self.reader = None;
        self.form.borrow_mut().take();
    }

    /// 分派确定、取消和应用。写入过程中不接受同一地址族的第二次提交。
    /// 错误：窗口已关闭，或无法启动提交任务。
    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        event: &Event,
    ) -> liteos_ui::Result<()> {
        let (id, domain, baseline, forward, close) = {
            let mut current = self.form.borrow_mut();
            let Some(form) = current.as_mut() else {
                return Ok(());
            };
            if self.writer.as_ref().is_some_and(|task| !task.is_finished()) {
                return Ok(());
            }
            match form.buttons.request(&form.frame, event) {
                Some(Request::Close) => {
                    drop(current);
                    self.close();
                    return Ok(());
                }
                Some(Request::Save { close }) => {
                    let Some(draft) = &form.draft else {
                        return Ok(());
                    };
                    let forward = form.forward.state()? & state::CHECKED != 0;
                    if forward == draft.forward {
                        if close {
                            drop(current);
                            self.close();
                        }
                        return Ok(());
                    }
                    form.forward
                        .set_state(form.forward.state()? | state::DISABLED)?;
                    form.buttons.set_enabled(false, false)?;
                    (
                        form.frame.id(),
                        form.domain.clone(),
                        draft.baseline.clone(),
                        forward,
                        close,
                    )
                }
                _ => return Ok(()),
            }
        };
        let family = self.family;
        let form = self.form.clone();
        let background = self.background.clone();
        let dialogs = app.dialogs();
        let parent = window.clone();
        self.writer = Some(app.spawn_local(window, async move {
            let result = async {
                let scope = domain.clone();
                let (revision, candidate) = background
                    .run(move || Draft::candidate(scope.as_deref(), family, baseline, forward))?
                    .await?;
                let applied = apply(&background, candidate.clone(), revision).await?;
                if !applied.saved {
                    return Err(io::Error::other(applied.message).into());
                }
                Ok::<_, Box<dyn std::error::Error>>(applied.saved_revision.map(|revision| Draft {
                    baseline: DraftBaseline::new(revision, candidate),
                    forward,
                }))
            }
            .await;
            let (message, icon) = match result {
                Ok(saved) => {
                    let mut current = form.borrow_mut();
                    let Some(visible) = matching(&mut current, id) else {
                        return;
                    };
                    if close {
                        current.take();
                        return;
                    }
                    if let Some(saved) = saved {
                        let _ = visible.publish(saved);
                        return;
                    }
                    // 已确认保存但没有确切的提交代际，旧草稿不能再次提交。
                    visible.draft = None;
                    let _ = visible.buttons.set_enabled(true, false);
                    (
                        "配置已保存。请重新打开设置以继续编辑。".to_owned(),
                        MessageIcon::Information,
                    )
                }
                Err(error) => {
                    if let Some(visible) = matching(&mut form.borrow_mut(), id) {
                        let _ = visible
                            .forward
                            .set_state(if forward { state::CHECKED } else { 0 });
                        let _ = visible.buttons.set_enabled(true, true);
                    }
                    (error.to_string(), MessageIcon::Error)
                }
            };
            if let Ok(alert) =
                dialogs.message(&parent, title(family), &message, MessageButtons::Ok, icon)
            {
                let _ = alert.await;
            }
        })?);
        Ok(())
    }
}

impl Form {
    fn publish(&mut self, draft: Draft) -> liteos_ui::Result<()> {
        self.forward
            .set_state(if draft.forward { state::CHECKED } else { 0 })?;
        self.buttons.set_enabled(true, true)?;
        self.draft = Some(draft);
        Ok(())
    }
}

fn matching(current: &mut Option<Form>, id: u64) -> Option<&mut Form> {
    current.as_mut().filter(|form| form.frame.id() == id)
}

impl Draft {
    fn load(domain: Option<&str>, family: Family) -> io::Result<Self> {
        let (revision, document) = ManagementClient::local().export()?;
        let mut config = RouterConfiguration::decode(&document).map_err(io::Error::other)?;
        Ok(Self {
            baseline: DraftBaseline::new(revision, document.into()),
            forward: *forwarding(&mut config, domain, family)?,
        })
    }

    fn candidate(
        domain: Option<&str>,
        family: Family,
        baseline: DraftBaseline,
        forward: bool,
    ) -> io::Result<(Revision, Arc<[u8]>)> {
        let (revision, document) = ManagementClient::local().export()?;
        baseline.verify(revision, &document)?;
        let mut config = RouterConfiguration::decode(&document).map_err(io::Error::other)?;
        *forwarding(&mut config, domain, family)? = forward;
        Ok((revision, config.encode().map_err(io::Error::other)?.into()))
    }
}

fn forwarding<'a>(
    config: &'a mut RouterConfiguration,
    domain: Option<&str>,
    family: Family,
) -> io::Result<&'a mut bool> {
    let (ipv4, ipv6) = match domain {
        Some(name) => {
            let domain = config
                .routing_domains
                .get_mut(name)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "所选路由域已删除。"))?;
            (&mut domain.ipv4_forwarding, &mut domain.ipv6_forwarding)
        }
        None => (&mut config.ipv4_forwarding, &mut config.ipv6_forwarding),
    };
    Ok(match family {
        Family::Ipv4 => ipv4,
        Family::Ipv6 => ipv6,
    })
}

fn title(family: Family) -> &'static str {
    match family {
        Family::Ipv4 => "IP 设置",
        Family::Ipv6 => "IPv6 设置",
    }
}
