//! 设备管理器：业务层枚举硬件，共享控件运行库负责树、菜单、属性页与窗口交互。
mod devices;
use devices::Category;
use liteos_ui::{
    Application, ControlFlow, EventKind, Menu, MenuItem, PropertySheet, TreeRow, Widget,
    WidgetKind as Kind, WidgetSpec, WindowOptions,
};
use std::{cell::RefCell, collections::BTreeMap, process::ExitCode, rc::Rc};
const APP_ID: &str = "org.liteos.device-manager";

#[derive(Default)]
struct Inventory {
    computer: String,
    categories: Vec<Category>,
    identities: BTreeMap<String, u64>,
    next: u64,
    devices: BTreeMap<u64, (usize, usize)>,
}
impl Inventory {
    fn publish(
        &mut self,
        computer: String,
        categories: Vec<Category>,
        tree: &Widget,
    ) -> liteos_ui::Result<()> {
        self.computer = computer;
        self.categories = categories;
        self.devices.clear();
        let mut identities = BTreeMap::new();
        let mut values = vec![(1, 0, self.computer.clone())];
        for (category_index, category) in self.categories.iter().enumerate() {
            let key = format!("category:{}", category.name);
            let category_id = self.identities.get(&key).copied().unwrap_or_else(|| {
                self.next += 1;
                self.next + 1
            });
            identities.insert(key, category_id);
            values.push((category_id, 1, category.name.to_owned()));
            for (device_index, device) in category.devices.iter().enumerate() {
                let key = format!("device:{}:{}:{device_index}", category.name, device.name);
                let id = self.identities.get(&key).copied().unwrap_or_else(|| {
                    self.next += 1;
                    self.next + 1
                });
                identities.insert(key, id);
                values.push((id, category_id, device.name.clone()));
                self.devices.insert(id, (category_index, device_index));
            }
        }
        self.identities = identities;
        tree.set_tree_rows(
            &values
                .iter()
                .map(|(id, parent, text)| TreeRow {
                    id: *id,
                    parent: *parent,
                    text,
                    expanded: true,
                    has_children: false,
                })
                .collect::<Vec<_>>(),
        )
    }
}
fn child(parent: &Widget, kind: Kind, text: &str, grow: u32) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout.grow = grow;
    parent.child(spec)
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = Application::new()?;
    let window = app.window(WindowOptions::new("设备管理器", APP_ID, 610, 460))?;
    let mut spec = WidgetSpec::new(Kind::Column, "");
    spec.layout.grow = 1;
    let root = window.widget(spec)?;
    let menu = child(&root, Kind::MenuBar, "", 0)?;
    menu.set_menus(&[
        Menu {
            title: "操作(A)",
            items: &[
                MenuItem {
                    id: 1,
                    label: "属性(R)",
                    shortcut: "Enter",
                    state: 0,
                },
                MenuItem {
                    id: 2,
                    label: "刷新(F)",
                    shortcut: "F5",
                    state: 4,
                },
                MenuItem {
                    id: 3,
                    label: "退出(X)",
                    shortcut: "",
                    state: 4,
                },
            ],
        },
        Menu {
            title: "查看(V)",
            items: &[MenuItem {
                id: 2,
                label: "刷新(R)",
                shortcut: "F5",
                state: 0,
            }],
        },
        Menu {
            title: "帮助(H)",
            items: &[MenuItem {
                id: 4,
                label: "关于设备管理器(A)",
                shortcut: "",
                state: 0,
            }],
        },
    ])?;
    let tree = child(&root, Kind::Tree, "", 1)?;
    let status = child(&root, Kind::Status, "正在扫描设备…", 0)?;
    let inventory = Rc::new(RefCell::new(Inventory::default()));
    let background = app.background()?;
    let mut scan = None;
    let mut properties: Option<(PropertySheet, Vec<Widget>)> = None;
    // 首次扫描也走后台；UI 不同步枚举设备和 /proc、/sys、/dev。
    let mut request_refresh = true;
    app.run(|app, event| {
        if let Some((sheet, _)) = &properties
            && event.window == sheet.window().id()
        {
            match event.kind {
                EventKind::Validate { token } => sheet.validated(token, Ok(()))?,
                EventKind::Apply { token } => sheet.applied(token, Ok(()))?,
                _ => {}
            }
            return Ok(ControlFlow::Continue);
        }
        let command = match event.kind {
            EventKind::Activate { value } if event.node == menu.id() => value,
            EventKind::Activate { .. } if event.node == tree.id() => 1,
            EventKind::Key {
                code: 63,
                pressed: true,
                handled: false,
                ..
            } => 2,
            _ => 0,
        };
        match command {
            1 => {
                let selected = tree.selection()?;
                let inventory = inventory.borrow();
                if let Some(&(category, device)) =
                    selected.and_then(|id| inventory.devices.get(&id))
                {
                    let device = &inventory.categories[category].devices[device];
                    let sheet = PropertySheet::new(
                        app,
                        Some(&window),
                        &format!("{} 属性", device.name),
                        440,
                        340,
                    )?;
                    let page = sheet.page("常规")?;
                    let title = child(&page, Kind::Label, &device.name, 0)?;
                    let details = child(&page, Kind::List, "", 1)?;
                    let rows: Vec<_> = device
                        .details
                        .iter()
                        .enumerate()
                        .map(|(index, (name, value))| {
                            (index as u64 + 1, format!("{name}：{value}"))
                        })
                        .collect();
                    details.set_rows(
                        &rows
                            .iter()
                            .map(|(id, text)| (*id, text.as_str()))
                            .collect::<Vec<_>>(),
                    )?;
                    let state = child(&page, Kind::Label, "这个设备运转正常。", 0)?;
                    properties = Some((sheet, vec![page, title, details, state]));
                }
            }
            2 => request_refresh = true,
            3 => return Ok(ControlFlow::Exit),
            4 => {
                let sheet = PropertySheet::new(app, Some(&window), "关于设备管理器", 360, 210)?;
                let page = sheet.page("设备管理器")?;
                let text = child(
                    &page,
                    Kind::Label,
                    "LiteOS 设备管理器\n查看系统设备及其属性。",
                    1,
                )?;
                text.set_state(liteos_ui::state::WRAP)?;
                properties = Some((sheet, vec![page, text]));
            }
            _ => {}
        }
        if request_refresh {
            request_refresh = false;
            // 替换 Task 撤销旧结果投递；已经开始的只读扫描自行清理，不阻塞 UI。
            scan.take();
            let work = background.run(|| {
                let computer = std::fs::read_to_string("/proc/sys/kernel/hostname")
                    .map(|name| name.trim().to_uppercase())
                    .unwrap_or_else(|_| "LITEOS".into());
                (computer, devices::scan())
            })?;
            let inventory = inventory.clone();
            let tree = tree.clone();
            let status = status.clone();
            status.set_text("正在扫描设备…")?;
            scan = Some(app.spawn_local(&window, async move {
                let (computer, categories) = work.await;
                let result = inventory.borrow_mut().publish(computer, categories, &tree);
                let message = match result {
                    Ok(()) => format!("{} 个设备", inventory.borrow().devices.len()),
                    Err(error) => error.to_string(),
                };
                let _ = status.set_text(&message);
            })?);
        }
        Ok(ControlFlow::Continue)
    })?;
    drop(scan);
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("devmgmt: {error}");
            ExitCode::FAILURE
        }
    }
}
