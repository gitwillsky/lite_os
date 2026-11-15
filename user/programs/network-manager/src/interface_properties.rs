//! WinBox 式接口属性窗口：在工作区内打开，标题为「接口 > 名称」，页签里是右对齐标签的表单，
//! 右侧「动作」面板打开地址设置，底部是运行状态徽标与「取消 / 应用 / 确定」。
//!
//! 「应用」「确定」按 WinBox 语义直接提交，不经差异审阅；提交以打开时读取的配置代际为基准，
//! 期间配置被他人修改会被拒绝。「应用」成功后重新读取，使下一次提交基于新的代际。
//! 列表工具栏的启用/停用同样直接提交。
use crate::{
    child,
    configuration::apply,
    item_list::spacer,
    routes::{
        ADMIN_STATES, Field, Fields, LiveInterface, admin_index, apply_fields, edit_ipv4_of,
        edit_ipv6_of,
    },
    workspace::Workspace,
};
use liteos_ui::{
    Application, Background, Dialogs, Event, EventKind, FontRole, Layout, MessageButtons,
    MessageIcon, Task, TreeRow, Widget, WidgetKind, WidgetSpec, Window, state,
};
use network_config::{configuration::RouterInterface, format_mac, traffic::Row};
use std::{cell::RefCell, rc::Rc, sync::Arc};

/// 表单标签列宽、输入框宽度与按钮尺寸（逻辑像素）。
const LABEL: i32 = 96;
const INPUT: i32 = 250;
const SIZE: (i32, i32) = (560, 400);

fn button() -> Layout {
    Layout {
        width: 75,
        height: 23,
        ..Layout::default()
    }
}

/// 所有打开的接口属性窗口与工具栏启停任务。
pub(super) struct InterfaceWindows {
    windows: Vec<PropertyWindow>,
    dialogs: Dialogs,
    background: Background,
    // 工具栏启停一次只进行一个；完成前的重复点击被忽略。
    toggle: Option<Task<()>>,
}

struct PropertyWindow {
    mac: [u8; 6],
    view: Rc<View>,
    // 读取、地址对话框或提交任务；进行中时忽略按钮，避免两份候选互相覆盖。
    task: Option<Task<()>>,
}

struct View {
    frame: Widget,
    /// 打开时的列表行：设备名与 MAC 用于重新读取，计数显示在「流量」页。
    row: Row,
    // 只需存活的容器与静态标签。
    _decor: Vec<Widget>,
    admin: Widget,
    display: Widget,
    mtu: Widget,
    preference: Widget,
    metric: Widget,
    ipv4: Widget,
    ipv6: Widget,
    badge: Widget,
    badge_text: Widget,
    message: Widget,
    cancel: Widget,
    apply: Widget,
    ok: Widget,
    /// 当前读取的配置基线；读取完成前为 None。
    live: RefCell<Option<LiveInterface>>,
    /// 尚未提交的地址修改；表单字段在提交时才解析进来。
    draft: RefCell<Option<RouterInterface>>,
}

/// 一行「右对齐标签 + 控件」。
fn field(
    parent: &Widget,
    decor: &mut Vec<Widget>,
    label: &str,
    kind: WidgetKind,
    text: &str,
    read_only: bool,
) -> liteos_ui::Result<Widget> {
    let line = child(
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
    decor.push(line.child(caption)?);
    let mut spec = WidgetSpec::new(kind, text);
    // 单行编辑框没有只读态；只显示的值用禁用态，与 WinBox 灰底的只读字段一致。
    spec.state = if read_only { state::DISABLED } else { 0 };
    spec.layout = Layout {
        width: INPUT,
        ..Layout::default()
    };
    let input = line.child(spec)?;
    decor.push(line);
    Ok(input)
}

impl View {
    fn new(frame: Widget, row: &Row) -> liteos_ui::Result<Self> {
        let mut decor = Vec::new();
        let root = child(
            &frame,
            WidgetKind::Column,
            "",
            Layout {
                grow: 1,
                padding: 4,
                gap: 4,
                ..Layout::default()
            },
        )?;
        let body = child(
            &root,
            WidgetKind::Row,
            "",
            Layout {
                grow: 1,
                gap: 4,
                ..Layout::default()
            },
        )?;
        let tabs = child(
            &body,
            WidgetKind::Tabs,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        let general = child(
            &tabs,
            WidgetKind::Column,
            "常规",
            Layout {
                padding: 10,
                gap: 5,
                ..Layout::default()
            },
        )?;
        let name = String::from_utf8_lossy(row.name()).into_owned();
        let admin = field(
            &general,
            &mut decor,
            "管理状态",
            WidgetKind::Combo,
            "",
            false,
        )?;
        admin.set_state(state::DROPDOWN_LIST)?;
        admin.set_tree_rows(
            &ADMIN_STATES
                .iter()
                .enumerate()
                .map(|(index, text)| TreeRow {
                    id: index as u64 + 1,
                    parent: 0,
                    text,
                    expanded: false,
                    has_children: false,
                })
                .collect::<Vec<_>>(),
        )?;
        let shown = field(&general, &mut decor, "名称", WidgetKind::Edit, &name, true)?;
        decor.push(shown);
        let display = field(
            &general,
            &mut decor,
            "显示名称",
            WidgetKind::Edit,
            "",
            false,
        )?;
        let shown = field(
            &general,
            &mut decor,
            "类型",
            WidgetKind::Edit,
            "以太网",
            true,
        )?;
        decor.push(shown);
        let mtu = field(&general, &mut decor, "MTU", WidgetKind::Edit, "", false)?;
        let mac = row.ethernet_address.map_or_else(String::new, format_mac);
        let shown = field(
            &general,
            &mut decor,
            "MAC 地址",
            WidgetKind::Edit,
            &mac,
            true,
        )?;
        decor.push(shown);
        let preference = field(
            &general,
            &mut decor,
            "网关优先级",
            WidgetKind::Edit,
            "",
            false,
        )?;
        let metric = field(
            &general,
            &mut decor,
            "网关度量",
            WidgetKind::Edit,
            "",
            false,
        )?;
        // 「流量」页是打开时的累计计数快照，实时速率在接口列表查看。
        let traffic = child(
            &tabs,
            WidgetKind::Column,
            "流量",
            Layout {
                padding: 10,
                gap: 5,
                ..Layout::default()
            },
        )?;
        let [
            rx_packets,
            tx_packets,
            rx_bytes,
            tx_bytes,
            rx_errors,
            tx_errors,
            rx_drop,
            tx_drop,
        ] = row.counters;
        for (label, value) in [
            ("接收报文", rx_packets),
            ("接收字节", rx_bytes),
            ("接收错误", rx_errors),
            ("接收丢包", rx_drop),
            ("发送报文", tx_packets),
            ("发送字节", tx_bytes),
            ("发送错误", tx_errors),
            ("发送丢包", tx_drop),
        ] {
            let shown = field(
                &traffic,
                &mut decor,
                label,
                WidgetKind::Edit,
                &value.to_string(),
                true,
            )?;
            decor.push(shown);
        }
        let panel = child(
            &body,
            WidgetKind::Column,
            "",
            Layout {
                width: 120,
                padding: 6,
                gap: 6,
                ..Layout::default()
            },
        )?;
        let title = child(&panel, WidgetKind::Label, "动作", Layout::default())?;
        title.set_font(FontRole::Message, None, Some(700))?;
        let ipv4 = child(&panel, WidgetKind::Link, "IPv4 设置…", Layout::default())?;
        let ipv6 = child(&panel, WidgetKind::Link, "IPv6 设置…", Layout::default())?;
        let message = child(&root, WidgetKind::Label, "正在读取…", Layout::default())?;
        let footer = child(
            &root,
            WidgetKind::Row,
            "",
            Layout {
                gap: 6,
                ..Layout::default()
            },
        )?;
        // 运行状态徽标：浅色底色加文字，对应 WinBox 底部左侧的 RUNNING 标记。
        let mut badge = WidgetSpec::new(WidgetKind::Row, "");
        badge.state = state::CLIENT_EDGE;
        badge.layout = Layout {
            padding: 2,
            ..Layout::default()
        };
        let badge = footer.child(badge)?;
        let badge_text = child(&badge, WidgetKind::Label, "", Layout::default())?;
        decor.push(spacer(&footer)?);
        let cancel = child(&footer, WidgetKind::Button, "取消", button())?;
        let apply = child(&footer, WidgetKind::Button, "应用", button())?;
        let ok = child(&footer, WidgetKind::Button, "确定", button())?;
        decor.extend([root, body, tabs, general, traffic, panel, title, footer]);
        let view = Self {
            frame,
            row: *row,
            _decor: decor,
            admin,
            display,
            mtu,
            preference,
            metric,
            ipv4,
            ipv6,
            badge,
            badge_text,
            message,
            cancel,
            apply,
            ok,
            live: RefCell::default(),
            draft: RefCell::default(),
        };
        view.show_state(row.up)?;
        view.set_ready(false)?;
        Ok(view)
    }

    /// 底部徽标显示当前管理状态。
    fn show_state(&self, up: bool) -> liteos_ui::Result<()> {
        let (text, color) = if up {
            ("运行中", 0x9BE29B)
        } else {
            ("已停用", 0xC8C8C8)
        };
        self.badge.set_background(Some(color), None)?;
        self.badge_text.set_text(text)
    }

    /// 读取或提交期间禁用输入和按钮；「取消」始终可用。
    fn set_ready(&self, ready: bool) -> liteos_ui::Result<()> {
        let bridge_port = self
            .live
            .borrow()
            .as_ref()
            .is_some_and(LiveInterface::bridge_port);
        let usable = |on: bool| if on { 0 } else { state::DISABLED };
        self.display.set_state(usable(ready))?;
        self.mtu.set_state(usable(ready))?;
        self.admin
            .set_state(state::DROPDOWN_LIST | usable(ready && !bridge_port))?;
        for widget in [&self.preference, &self.metric, &self.ipv4, &self.ipv6] {
            widget.set_state(usable(ready && !bridge_port))?;
        }
        self.apply.set_state(usable(ready))?;
        self.ok.set_state(if ready {
            state::DEFAULT
        } else {
            state::DISABLED
        })
    }

    /// 以读取到的配置填充表单。
    fn present(&self, live: LiveInterface) -> liteos_ui::Result<()> {
        let draft = &live.initial;
        self.admin
            .select(Some(admin_index(draft.enabled) as u64 + 1))?;
        self.display
            .set_text(draft.display_name.as_deref().unwrap_or(""))?;
        self.mtu.set_text(&draft.mtu.to_string())?;
        let route = draft.default_route;
        self.preference
            .set_text(&route.map_or_else(String::new, |value| value.preference.to_string()))?;
        self.metric
            .set_text(&route.map_or_else(String::new, |value| value.metric.to_string()))?;
        let note = if live.bridge_port() {
            "此设备是网桥端口，管理状态与三层地址在网桥和 VLAN 网关设置。"
        } else {
            "网关优先级与度量留空时使用系统策略。"
        };
        self.message.set_text(note)?;
        *self.draft.borrow_mut() = Some(draft.clone());
        *self.live.borrow_mut() = Some(live);
        self.set_ready(true)
    }

    /// 解析表单得到待提交的配置；输入无效时提示并聚焦相应字段。
    fn collect(&self) -> liteos_ui::Result<Option<RouterInterface>> {
        let live = self.live.borrow();
        let (Some(live), Some(mut next)) = (live.as_ref(), self.draft.borrow().clone()) else {
            return Ok(None);
        };
        let fields = Fields {
            display: &self.display.text()?,
            mtu: &self.mtu.text()?,
            admin: self.admin.selection()?.map(|id| (id - 1) as usize),
            preference: &self.preference.text()?,
            metric: &self.metric.text()?,
        };
        if let Err((field, message)) = apply_fields(&mut next, &fields, live.bridge_port()) {
            self.message.set_text(message)?;
            match field {
                Field::Display => self.display.focus()?,
                Field::Mtu => self.mtu.focus()?,
                Field::Route => self.preference.focus()?,
            }
            return Ok(None);
        }
        Ok(Some(next))
    }
}

/// 后台读取网卡配置并填充窗口；失败时在提示行显示原因，表单保持禁用。
async fn load(view: Rc<View>, background: Background, domain: Option<Arc<str>>, row: Row) {
    view.live.borrow_mut().take();
    let _ = view.set_ready(false);
    let _ = view.message.set_text("正在读取…");
    let result = match background.run(move || LiveInterface::load(domain.as_deref(), &row)) {
        Ok(task) => task.await.map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    let _ = match result {
        Ok(live) => view.present(live),
        Err(error) => view.message.set_text(&error),
    };
}

/// 以 `live` 为基线把 `next` 直接提交；返回是否已保存及提示。
async fn submit(
    background: &Background,
    live: LiveInterface,
    next: RouterInterface,
) -> (bool, String) {
    let revision = live.revision;
    let candidate = match background.run(move || live.candidate(next)) {
        Ok(task) => task.await,
        Err(error) => return (false, error.to_string()),
    };
    let candidate = match candidate {
        Ok(candidate) => candidate,
        Err(error) => return (false, error.to_string()),
    };
    match apply(background, candidate, revision).await {
        Ok(result) => (result.saved, result.message),
        Err(error) => (false, error.to_string()),
    }
}

impl InterfaceWindows {
    pub(super) fn new(app: &Application, background: Background) -> Self {
        Self {
            windows: Vec::new(),
            dialogs: app.dialogs(),
            background,
            toggle: None,
        }
    }

    /// 打开 `row` 的属性窗口；同一网卡已打开时把它置前。
    ///
    /// 错误：窗口已关闭或无法建立读取任务。
    pub(super) fn open(
        &mut self,
        app: &mut Application,
        window: &Window,
        workspace: &Workspace,
        domain: Option<Arc<str>>,
        row: Row,
    ) -> liteos_ui::Result<()> {
        let Some(mac) = row.ethernet_address else {
            return Ok(());
        };
        if let Some(open) = self.windows.iter().find(|open| open.mac == mac) {
            return workspace.raise(&open.view.frame);
        }
        let title = format!("接口 > {}", String::from_utf8_lossy(row.name()));
        let view = Rc::new(View::new(workspace.item_window(&title, SIZE)?, &row)?);
        let task = app.spawn_local(
            window,
            load(view.clone(), self.background.clone(), domain, row),
        )?;
        self.windows.push(PropertyWindow {
            mac,
            view,
            task: Some(task),
        });
        Ok(())
    }

    /// 工具栏的启用/停用：读取当前配置，只改管理状态并直接提交。失败时弹出提示。
    ///
    /// 错误：窗口已关闭或无法建立任务。
    pub(super) fn set_enabled(
        &mut self,
        app: &mut Application,
        window: &Window,
        domain: Option<Arc<str>>,
        row: Row,
        enabled: bool,
    ) -> liteos_ui::Result<()> {
        if self.toggle.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        let background = self.background.clone();
        let dialogs = self.dialogs.clone();
        let parent = window.clone();
        self.toggle = Some(app.spawn_local(window, async move {
            let loaded = match background.run(move || LiveInterface::load(domain.as_deref(), &row))
            {
                Ok(task) => task.await.map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let message = match loaded {
                Ok(live) if live.bridge_port() => {
                    Some("请在「网桥」中管理此端口的转发状态。".to_owned())
                }
                Ok(live) => {
                    let next = RouterInterface {
                        enabled: Some(enabled),
                        ..live.initial.clone()
                    };
                    let (saved, message) = submit(&background, live, next).await;
                    (!saved).then_some(message)
                }
                Err(error) => Some(error),
            };
            if let Some(message) = message
                && let Ok(notice) = dialogs.message(
                    &parent,
                    if enabled {
                        "启用接口"
                    } else {
                        "停用接口"
                    },
                    &message,
                    MessageButtons::Ok,
                    MessageIcon::Error,
                )
            {
                let _ = notice.await;
            }
        })?);
        Ok(())
    }

    /// 处理属性窗口的按钮、动作与关闭。
    ///
    /// 错误：窗口已关闭或无法建立任务。
    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        event: &Event,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        let Some(index) = self.windows.iter().position(|open| {
            let view = &open.view;
            [
                &view.frame,
                &view.cancel,
                &view.apply,
                &view.ok,
                &view.ipv4,
                &view.ipv6,
            ]
            .iter()
            .any(|widget| widget.id() == event.node)
        }) else {
            return Ok(());
        };
        let view = self.windows[index].view.clone();
        // 关闭按钮、Ctrl+F4 与「取消」丢弃未提交的修改；进行中的任务随窗口一起取消。
        if event.node == view.frame.id() || event.node == view.cancel.id() {
            self.windows.remove(index);
            return Ok(());
        }
        let open = &mut self.windows[index];
        if open.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        let background = self.background.clone();
        let dialogs = self.dialogs.clone();
        let parent = window.clone();
        if event.node == view.ipv4.id() || event.node == view.ipv6.id() {
            let ipv4 = event.node == view.ipv4.id();
            open.task = Some(app.spawn_local(window, async move {
                let (Some(mut draft), Some((device, named))) = (
                    view.draft.borrow().clone(),
                    view.live
                        .borrow()
                        .as_ref()
                        .map(|live| (live.device.clone(), live.named())),
                ) else {
                    return;
                };
                let result = if ipv4 {
                    edit_ipv4_of(&dialogs, &parent, &mut draft, device.as_ref(), named).await
                } else {
                    edit_ipv6_of(&dialogs, &parent, &mut draft).await
                };
                if result.is_ok() {
                    *view.draft.borrow_mut() = Some(draft);
                }
            })?);
            return Ok(());
        }
        let close = event.node == view.ok.id();
        let Some(next) = view.collect()? else {
            return Ok(());
        };
        let live = view
            .live
            .borrow()
            .clone()
            .expect("collected from a loaded form");
        // WinBox：没有改动时「确定」只关闭窗口，「应用」不提交。
        if next == live.initial {
            if close {
                self.windows.remove(index);
            } else {
                view.message.set_text("配置没有改动")?;
            }
            return Ok(());
        }
        let row = view.row;
        view.set_ready(false)?;
        view.message.set_text("正在提交…")?;
        let finished = view.clone();
        open.task = Some(app.spawn_local(window, async move {
            let (saved, message) = submit(&background, live, next).await;
            if saved && close {
                // 由下一次事件分派移除窗口；任务内不能释放持有自身的窗口。
                let _ = finished.frame.set_state(state::HIDDEN);
                return;
            }
            if saved {
                load(finished.clone(), background, domain, row).await;
            } else {
                let _ = finished.set_ready(true);
            }
            let _ = finished.message.set_text(&message);
        })?);
        Ok(())
    }

    /// 移除提交「确定」后已隐藏的窗口。
    pub(super) fn prune(&mut self) {
        self.windows.retain(|open| {
            open.view
                .frame
                .state()
                .is_ok_and(|value| value & state::HIDDEN == 0)
        });
    }
}
