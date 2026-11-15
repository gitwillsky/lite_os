//! 「IP → 地址」列表窗口：所选路由域各网卡的 IPv4 地址、网段、网关与所属接口。
//!
//! 默认域读取运行快照，标志 D 表示经 DHCP 取得；具名域读取已保存配置中的静态地址。每两秒重新读取。
//! 按 WinBox 语义，双击打开该网卡的 IPv4 设置、「删除」清除其地址设置，两者都直接提交；
//! 「新建」打开路由接口设置（每块网卡一份 IPv4 配置，新地址在对应网卡上设置）。
use crate::{
    interface_properties::submit,
    item_list::{Body, Glyph, Item, Summary, Tool, toolbar},
    routes::{Dialog, LiveInterface, Section, edit_ipv4_of},
};
use liteos_ui::{
    Application, Background, Clock, Column, Dialogs, Event, EventKind, ListOptions, ListStyle,
    Task, Widget, Window,
};
use network_config::{
    ManagementClient,
    configuration::{Assignment, DeviceMatch, RouterConfiguration},
    traffic::Row,
};
use std::{cell::RefCell, io, net::Ipv4Addr, rc::Rc, sync::Arc, time::Duration};

/// 一条地址；`mac` 缺席时（按设备名绑定的具名域配置）只显示、不可编辑。
#[derive(Clone)]
struct Entry {
    dynamic: bool,
    address: Ipv4Addr,
    prefix: u8,
    gateway: Ipv4Addr,
    interface: String,
    device: Vec<u8>,
    mac: Option<[u8; 6]>,
}

impl Entry {
    /// 交给接口配置读取的最小列表行：只用到设备名与 MAC。
    fn row(&self) -> Option<Row> {
        let mut name = [0; 16];
        let length = self.device.len().min(16);
        name[..length].copy_from_slice(&self.device[..length]);
        Some(Row {
            index: 0,
            name,
            ethernet_address: Some(self.mac?),
            up: true,
            counters: [0; 8],
            bytes_per_second: None,
        })
    }
}

/// 在后台线程读取所选域的地址。
fn load(domain: Option<&str>) -> io::Result<Vec<Entry>> {
    let mut client = ManagementClient::local();
    let Some(domain) = domain else {
        let snapshot = client.snapshot()?;
        return Ok(snapshot
            .interfaces
            .into_iter()
            .filter(|state| !state.address.is_unspecified())
            .map(|state| Entry {
                dynamic: state.lease.is_some(),
                address: state.address,
                prefix: state.prefix_length,
                gateway: state.gateway,
                interface: String::from_utf8_lossy(&state.name).into_owned(),
                device: state.name,
                mac: Some(state.mac),
            })
            .collect());
    };
    let (_, bytes) = client.export()?;
    let config = RouterConfiguration::decode(&bytes).map_err(io::Error::other)?;
    let scope = config
        .routing_domains
        .iter()
        .find(|(name, _)| name.as_str() == domain)
        .map(|(_, scope)| scope)
        .ok_or_else(|| io::Error::other("所选路由域已删除。"))?;
    Ok(scope
        .interfaces
        .iter()
        .filter_map(|(name, interface)| {
            let ipv4 = interface.ipv4.as_ref()?;
            (ipv4.assignment == Assignment::Static && !ipv4.address.is_unspecified()).then(|| {
                let (device, mac) = match &interface.device {
                    DeviceMatch::Mac(mac) => (name.as_str().as_bytes().to_vec(), Some(*mac)),
                    DeviceMatch::Name(device) => (device.as_str().as_bytes().to_vec(), None),
                };
                Entry {
                    dynamic: false,
                    address: ipv4.address,
                    prefix: ipv4.prefix_length,
                    gateway: ipv4.gateway,
                    interface: name.as_str().to_owned(),
                    device,
                    mac,
                }
            })
        })
        .collect())
}

fn network(address: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    Ipv4Addr::from(u32::from(address) & mask)
}

fn row_text(entry: &Entry) -> String {
    let gateway = if entry.gateway.is_unspecified() {
        String::new()
    } else {
        entry.gateway.to_string()
    };
    format!(
        "{}\t{}/{}\t{}\t{}\t{gateway}",
        if entry.dynamic { "D" } else { "" },
        entry.address,
        entry.prefix,
        network(entry.address, entry.prefix),
        entry.interface,
    )
}

pub(super) struct Page {
    view: Rc<View>,
    task: Option<Task<()>>,
    // 地址对话框与提交任务；进行中时忽略新的编辑请求。
    edit: Option<Task<()>>,
    visible: bool,
    domain: Option<Arc<str>>,
    dialogs: Dialogs,
}

struct View {
    _body: Body,
    list: Widget,
    add: Tool,
    remove: Tool,
    refresh: Tool,
    summary: Summary,
    background: Background,
    clock: Clock,
    entries: RefCell<Vec<Entry>>,
}

impl View {
    fn selected(&self) -> liteos_ui::Result<Option<Entry>> {
        let selected = self.list.selection()?;
        Ok(selected.and_then(|id| self.entries.borrow().get(id as usize - 1).cloned()))
    }

    fn show(&self, note: &str) -> liteos_ui::Result<()> {
        let selected = self.selected()?;
        self.remove
            .set_enabled(selected.as_ref().is_some_and(|entry| entry.mac.is_some()))?;
        self.summary.show(
            self.entries.borrow().len(),
            usize::from(selected.is_some()),
            note,
        )
    }

    fn present(&self, entries: Vec<Entry>) -> liteos_ui::Result<()> {
        let previous = self.selected()?.map(|entry| entry.device);
        let text: Vec<_> = entries.iter().map(row_text).collect();
        let rows: Vec<_> = text
            .iter()
            .enumerate()
            .map(|(index, text)| (index as u64 + 1, text.as_str()))
            .collect();
        self.list.set_rows(&rows)?;
        let selection = previous.and_then(|device| {
            entries
                .iter()
                .position(|entry| entry.device == device)
                .map(|index| index as u64 + 1)
        });
        self.list.select(selection)?;
        *self.entries.borrow_mut() = entries;
        self.show("实时")
    }

    async fn observe(&self, domain: Option<Arc<str>>) {
        loop {
            let scope = domain.clone();
            let loaded = match self.background.run(move || load(scope.as_deref())) {
                Ok(task) => task.await,
                Err(error) => Err(io::Error::other(error.to_string())),
            };
            let shown = match loaded {
                Ok(entries) => self.present(entries),
                Err(error) => self
                    .summary
                    .notice(&format!("地址读取失败，请刷新。{error}")),
            };
            if shown.is_err() {
                return;
            }
            let Ok(sleep) = self.clock.sleep(Duration::from_secs(2)) else {
                return;
            };
            sleep.await;
        }
    }
}

impl Page {
    pub(super) fn new(
        app: &Application,
        parent: &Widget,
        background: Background,
    ) -> liteos_ui::Result<Self> {
        let [add, remove, _, refresh] = toolbar(
            parent,
            [
                Item::Command(Glyph::Add, "新建"),
                Item::Command(Glyph::Remove, "删除"),
                Item::Separator,
                Item::Icon(Glyph::Refresh, "刷新"),
            ],
        )?;
        remove.set_enabled(false)?;
        let body = Body::new(parent)?;
        let list = body.list.clone();
        list.set_list_style(
            ListStyle::Details,
            ListOptions {
                multiple: false,
                full_row_select: true,
            },
        )?;
        let columns = [
            ("", 26),
            ("地址", 150),
            ("网段", 120),
            ("接口", 110),
            ("网关", 120),
        ];
        list.set_columns(&std::array::from_fn::<_, 5, _>(|index| Column {
            id: index as u64 + 1,
            title: columns[index].0,
            width: columns[index].1,
        }))?;
        let summary = Summary::new(parent)?;
        Ok(Self {
            view: Rc::new(View {
                _body: body,
                list,
                add,
                remove,
                refresh,
                summary,
                background,
                clock: app.clock(),
                entries: RefCell::default(),
            }),
            task: None,
            edit: None,
            visible: false,
            domain: None,
            dialogs: app.dialogs(),
        })
    }

    pub(super) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        self.domain = domain;
        if self.visible {
            self.subscribe(app, window)?;
        }
        Ok(())
    }

    pub(super) fn selected_tab(
        &mut self,
        app: &mut Application,
        window: &Window,
        visible: bool,
    ) -> liteos_ui::Result<()> {
        if self.visible == visible {
            return Ok(());
        }
        self.visible = visible;
        if visible {
            self.subscribe(app, window)
        } else {
            self.task = None;
            Ok(())
        }
    }

    fn subscribe(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        self.view.summary.notice("正在读取地址…")?;
        let view = self.view.clone();
        let domain = self.domain.clone();
        self.task = Some(app.spawn_local(window, async move {
            view.observe(domain).await;
        })?);
        Ok(())
    }

    /// 「新建」请求打开的配置对话框。
    pub(super) fn request(&self, event: &Event) -> Option<Dialog> {
        (self.visible && self.view.add.activated(event))
            .then_some(Dialog::Settings(Some(Section::Interfaces)))
    }

    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        event: &Event,
    ) -> liteos_ui::Result<()> {
        if !self.visible {
            return Ok(());
        }
        let view = &self.view;
        if event.node == view.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            return view.show("实时");
        }
        if view.refresh.activated(event) {
            return self.subscribe(app, window);
        }
        let remove = view.remove.activated(event);
        if !(remove
            || event.node == view.list.id() && matches!(event.kind, EventKind::Activate { .. }))
            || self.edit.as_ref().is_some_and(|task| !task.is_finished())
        {
            return Ok(());
        }
        let Some(row) = view.selected()?.and_then(|entry| entry.row()) else {
            return Ok(());
        };
        let view = view.clone();
        let domain = self.domain.clone();
        let dialogs = self.dialogs.clone();
        let parent = window.clone();
        self.edit = Some(app.spawn_local(window, async move {
            let background = view.background.clone();
            let loaded = match background.run(move || LiveInterface::load(domain.as_deref(), &row))
            {
                Ok(task) => task.await,
                Err(error) => Err(io::Error::other(error.to_string())),
            };
            let live = match loaded {
                Ok(live) => live,
                Err(error) => {
                    let _ = view.summary.notice(&error.to_string());
                    return;
                }
            };
            let mut next = live.initial.clone();
            if remove {
                next.ipv4 = None;
            } else if edit_ipv4_of(
                &dialogs,
                &parent,
                &mut next,
                live.device.as_ref(),
                live.named(),
            )
            .await
            .is_err()
            {
                return;
            }
            if next == live.initial {
                return;
            }
            let (_, message) = submit(&background, live, next).await;
            let _ = view.summary.notice(&message);
        })?);
        Ok(())
    }
}
