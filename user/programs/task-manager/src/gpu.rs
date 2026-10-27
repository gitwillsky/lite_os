//! GPU 页采样标准 DRM 客户端统计；不以虚拟队列等待时间冒充 GPU 执行时间。
use crate::{child, performance::stats, processes::Process, system::memory_text};
use liteos_ui::{Layout, Widget, WidgetKind as Kind};
use std::{collections::BTreeMap, fs};

/// 后台采集的实际设备和 DRM 客户端快照。
#[derive(Clone, Default)]
pub struct Snapshot {
    /// （DRM 节点名, 驱动名），按节点名排序。
    devices: Vec<(String, String)>,
    clients: BTreeMap<u64, Client>,
}

#[derive(Clone)]
struct Client {
    pid: u32,
    name: String,
    resident_kib: Option<u64>,
    /// `drm-shared-memory`：同时被其他 DRM file 持有的缓冲，已计入 `resident_kib`。
    shared_kib: Option<u64>,
}

impl Snapshot {
    /// 从实际 DRM 设备与进程读取快照；权限不足或采样中退出的进程不伪造为零占用。
    ///
    /// # 参数
    /// - `processes`：同一次后台采样取得的进程清单。
    ///
    /// # 返回
    /// 当前可读取的设备和按 DRM client-id 去重的客户端；缺失的内存字段保持 None。
    pub fn read(processes: &[Process]) -> Self {
        let mut snapshot = Self::default();
        if let Ok(entries) = fs::read_dir("/sys/class/drm") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(index) = name
                    .strip_prefix("card")
                    .and_then(|id| id.parse::<u32>().ok())
                else {
                    continue;
                };
                let driver = fs::read_to_string(entry.path().join("device/uevent"))
                    .ok()
                    .and_then(|text| {
                        text.lines()
                            .find_map(|line| line.strip_prefix("DRIVER=").map(str::to_owned))
                    })
                    .unwrap_or_default();
                snapshot.devices.push((format!("card{index}"), driver));
            }
        }
        snapshot.devices.sort();
        // 1. 先检查 fd 的实际设备路径，避免格式化无关文件、inotify 等大型 fdinfo。
        // 2. client-id 属于实际 DRM OFD；dup/fork 与跨进程共享 descriptor 只显示一次。
        for process in processes {
            let base = format!("/proc/{}", process.pid);
            let Ok(entries) = fs::read_dir(format!("{base}/fd")) else {
                continue;
            };
            for entry in entries.flatten() {
                if !fs::read_link(entry.path()).is_ok_and(|path| path.starts_with("/dev/dri")) {
                    continue;
                }
                let Ok(text) = fs::read_to_string(format!(
                    "{base}/fdinfo/{}",
                    entry.file_name().to_string_lossy()
                )) else {
                    continue;
                };
                snapshot.observe(process.pid, &process.name, &text);
            }
        }
        snapshot
    }

    fn observe(&mut self, pid: u32, name: &str, text: &str) {
        let field = |key| {
            text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                (name == key).then_some(value.trim())
            })
        };
        if field("drm-driver").is_none() {
            return;
        }
        let Some(id) = field("drm-client-id").and_then(|value| value.parse().ok()) else {
            return;
        };
        let kib = |key| {
            field(key).and_then(|value| {
                let mut parts = value.split_whitespace();
                let number: u64 = parts.next()?.parse().ok()?;
                match parts.next() {
                    None => Some(number / 1024),
                    Some("KiB") => Some(number),
                    Some("MiB") => number.checked_mul(1024),
                    _ => None,
                }
            })
        };
        let resident_kib = kib("drm-resident-memory");
        let shared_kib = kib("drm-shared-memory");
        self.clients
            .entry(id)
            .and_modify(|client| {
                if pid < client.pid {
                    client.pid = pid;
                    client.name = name.to_owned();
                }
            })
            .or_insert_with(|| Client {
                pid,
                name: name.to_owned(),
                resident_kib,
                shared_kib,
            });
    }
}

/// GPU 客户端列表的列，顺序即显示顺序。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    Pid,
    Resident,
    Shared,
}

impl Column {
    pub const ALL: [Column; 4] = [Column::Name, Column::Pid, Column::Resident, Column::Shared];

    fn title(self) -> &'static str {
        match self {
            Column::Name => "映像名称",
            Column::Pid => "PID",
            Column::Resident => "缓冲占用",
            Column::Shared => "其中共享",
        }
    }

    fn width(self) -> i32 {
        match self {
            Column::Name => 180,
            Column::Pid => 50,
            Column::Resident => 110,
            Column::Shared => 110,
        }
    }

    fn text(self, client: &Client) -> String {
        match self {
            Column::Name => client.name.clone(),
            Column::Pid => client.pid.to_string(),
            Column::Resident => client.resident_kib.map(memory_text).unwrap_or_default(),
            Column::Shared => client.shared_kib.map(memory_text).unwrap_or_default(),
        }
    }
}

/// 客户端列表的排序，交互与进程页一致：同列翻转，换列时数值列从大到小。默认按缓冲占用从大到小。
#[derive(Clone, Copy)]
pub struct Sort {
    column: Column,
    descending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            column: Column::Resident,
            descending: true,
        }
    }
}

impl Sort {
    pub fn click(&mut self, column: Column) {
        if self.column == column {
            self.descending = !self.descending;
        } else {
            *self = Self {
                column,
                descending: matches!(column, Column::Resident | Column::Shared),
            };
        }
    }

    fn apply(self, rows: &mut [(u64, &Client)]) {
        rows.sort_by(|(_, a), (_, b)| {
            let order = match self.column {
                Column::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                Column::Pid => a.pid.cmp(&b.pid),
                Column::Resident => a.resident_kib.cmp(&b.resident_kib),
                Column::Shared => a.shared_kib.cmp(&b.shared_kib),
            };
            let order = if self.descending {
                order.reverse()
            } else {
                order
            };
            order.then(a.pid.cmp(&b.pid))
        });
    }
}

/// GPU 标签页：仿性能页的统计分组与整行选择的客户端列表；只展示已采集数据。
pub struct View {
    /// 名称、驱动程序。
    device: [Widget; 2],
    /// 客户端数、私有缓冲合计。
    totals: [Widget; 2],
    clients: Widget,
    group: Widget,
    note: Widget,
    _containers: Vec<Widget>,
}

impl View {
    pub fn new(page: &Widget) -> liteos_ui::Result<Self> {
        let mut containers = Vec::new();
        let row = child(
            page,
            Kind::Row,
            "",
            Layout {
                gap: 8,
                ..Default::default()
            },
        )?;
        let column = |containers: &mut Vec<Widget>| {
            let column = child(
                &row,
                Kind::Column,
                "",
                Layout {
                    grow: 1,
                    ..Default::default()
                },
            )?;
            containers.push(column.clone());
            Ok::<_, liteos_ui::Error>(column)
        };
        let left = column(&mut containers)?;
        let right = column(&mut containers)?;
        let device = stats(&left, "设备", ["名称", "驱动程序"], &mut containers)?;
        let totals = stats(&right, "总数", ["客户端", "私有缓冲"], &mut containers)?;
        containers.push(row);
        let group = child(
            page,
            Kind::Group,
            "GPU 客户端",
            Layout {
                grow: 1,
                padding: 6,
                ..Default::default()
            },
        )?;
        let clients = child(
            &group,
            Kind::List,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        // 与进程页相同的 Details 视图与整行选择；排序由 Column 事件交给 Sort。
        clients.set_list_style(
            liteos_ui::ListStyle::Details,
            liteos_ui::ListOptions {
                multiple: false,
                full_row_select: true,
            },
        )?;
        clients.set_columns(
            &Column::ALL
                .iter()
                .enumerate()
                .map(|(index, column)| liteos_ui::Column {
                    id: index as u64 + 1,
                    title: column.title(),
                    width: column.width(),
                })
                .collect::<Vec<_>>(),
        )?;
        for (index, column) in Column::ALL.iter().enumerate() {
            if *column != Column::Name {
                clients
                    .set_column_alignment(index as u64 + 1, liteos_ui::ColumnAlignment::Right)?;
            }
        }
        let note = child(
            page,
            Kind::Label,
            "缓冲位于系统内存；共享缓冲计入每个使用它的客户端，不能相加。",
            Layout {
                height: 20,
                ..Default::default()
            },
        )?;
        Ok(Self {
            device,
            totals,
            clients,
            group,
            note,
            _containers: containers,
        })
    }

    pub fn clients(&self) -> &Widget {
        &self.clients
    }

    pub fn update(&self, snapshot: &Snapshot, sort: Sort) -> liteos_ui::Result<()> {
        let join = |part: fn(&(String, String)) -> &str| {
            snapshot
                .devices
                .iter()
                .map(part)
                .collect::<Vec<_>>()
                .join("，")
        };
        if snapshot.devices.is_empty() {
            self.device[0].set_text("未发现")?;
            self.device[1].set_text("")?;
        } else {
            self.device[0].set_text(&join(|device| &device.0))?;
            self.device[1].set_text(&join(|device| &device.1))?;
        }
        let mut rows: Vec<_> = snapshot
            .clients
            .iter()
            .filter(|(_, client)| client.resident_kib.is_some())
            .map(|(id, client)| (*id, client))
            .collect();
        // 私有部分只属于一个客户端，可以精确相加；缺少共享统计时不推算合计。
        let private = rows.iter().try_fold(0u64, |sum, (_, client)| {
            Some(sum + client.resident_kib?.saturating_sub(client.shared_kib?))
        });
        self.totals[0].set_text(&rows.len().to_string())?;
        self.totals[1].set_text(&private.map(memory_text).unwrap_or_default())?;
        // 未采到缓冲统计时隐藏整组及其说明，避免空表头与缺失指标占位。
        let visibility = if rows.is_empty() {
            liteos_ui::state::HIDDEN
        } else {
            0
        };
        self.group.set_state(visibility)?;
        self.note.set_state(visibility)?;
        sort.apply(&mut rows);
        let text: Vec<_> = rows
            .iter()
            .map(|(_, client)| {
                Column::ALL
                    .iter()
                    .map(|column| column.text(client))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect();
        self.clients.set_rows(
            &rows
                .iter()
                .zip(&text)
                .map(|((id, _), text)| (*id, text.as_str()))
                .collect::<Vec<_>>(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Snapshot;

    #[test]
    fn shared_descriptors_are_one_client_and_missing_memory_is_not_zero() {
        let mut snapshot = Snapshot::default();
        let info = "drm-driver: virtio_gpu\ndrm-client-id: 7\ndrm-shared-memory: 4 KiB\ndrm-resident-memory: 4 MiB\n";
        snapshot.observe(20, "child", info);
        snapshot.observe(10, "parent", info);
        snapshot.observe(10, "parent", "drm-driver: virtio_gpu\ndrm-client-id: 8\n");
        assert_eq!(snapshot.clients.len(), 2);
        assert_eq!(snapshot.clients[&7].pid, 10);
        assert_eq!(snapshot.clients[&7].resident_kib, Some(4096));
        assert_eq!(snapshot.clients[&7].shared_kib, Some(4));
        assert_eq!(snapshot.clients[&8].resident_kib, None);
        assert_eq!(snapshot.clients[&8].shared_kib, None);
    }
}
