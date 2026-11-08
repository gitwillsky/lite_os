//! 外壳批量事务及其撤销记录；只在命令时计算排列，不进入呈现热路径。
use super::{Geometry, Mode, Placement, Stack, Visibility};
use crate::wayland::ClientId;
use wayland_wire::{protocol::liteos_shell_v1::Arrangement, wire::ObjectId};

/// `(最小尺寸, 最大尺寸)`，逻辑像素；各维度的 0 表示无限制。
pub type SizeLimits = ((i32, i32), (i32, i32));

/// 一份历史命令的输入，不是当前几何的第二个可写 owner；手动修改与 unmap 撤销对应成员。
struct Saved {
    client: ClientId,
    surface: ObjectId,
    geometry: Geometry,
    mode: Mode,
    restore: Option<Geometry>,
}

enum Operation {
    Minimized {
        showing_desktop: bool,
    },
    Arranged {
        layout: Arrangement,
        saved: Vec<Saved>,
    },
}

pub(super) struct ShellBatch {
    owner: ClientId,
    operation: Operation,
}

impl Stack {
    /// 最近批量操作的授权连接；最后一个控制对象退出时必须撤销。
    pub fn batch_owner(&self) -> Option<ClientId> {
        self.batch.as_ref().map(|batch| batch.owner)
    }

    /// 桌面是否已提升至普通窗口之上；Top/Overlay 外壳不受影响。
    pub fn showing_desktop(&self) -> bool {
        self.batch.as_ref().is_some_and(|batch| {
            matches!(
                batch.operation,
                Operation::Minimized {
                    showing_desktop: true
                }
            )
        })
    }

    /// 指定连接能否撤销它最近一次批量最小化。
    pub fn can_undo(&self, owner: ClientId) -> bool {
        self.batch.as_ref().is_some_and(|batch| {
            batch.owner == owner && matches!(batch.operation, Operation::Minimized { .. })
        }) && self
            .order
            .iter()
            .any(|placed| placed.visibility == Visibility::BatchMinimized)
    }

    /// 可撤销排列的类型；其他连接、无成员或当前命令是最小化时返回 None，无错误返回。
    pub fn undo_arrangement_kind(&self, owner: ClientId) -> Option<Arrangement> {
        let batch = self.batch.as_ref().filter(|batch| batch.owner == owner)?;
        match &batch.operation {
            Operation::Arranged { layout, saved } if !saved.is_empty() => Some(*layout),
            _ => None,
        }
    }

    /// 普通窗口是否位于当前桌面之上；绘制和输入共用此判定。
    pub fn visible(&self, placed: &Placement) -> bool {
        !placed.minimized() && !self.showing_desktop()
    }

    /// 原子最小化本次可见窗口；空操作保留前次撤销来源，不分配 ID 快照。
    pub fn minimize_all(&mut self, owner: ClientId) -> bool {
        self.begin_batch(owner, false)
    }

    fn begin_batch(&mut self, owner: ClientId, showing_desktop: bool) -> bool {
        if !showing_desktop && !self.order.iter().any(|placed| !placed.minimized()) {
            return false;
        }
        for placed in &mut self.order {
            placed.visibility = match placed.visibility {
                Visibility::Visible => Visibility::BatchMinimized,
                Visibility::BatchMinimized | Visibility::Minimized => Visibility::Minimized,
            };
        }
        self.batch = Some(ShellBatch {
            owner,
            operation: Operation::Minimized { showing_desktop },
        });
        true
    }

    /// 只还原仍属于本连接批次的窗口，保留栈序及后来的手动最小化。
    pub fn undo_minimize_all(&mut self, owner: ClientId) -> bool {
        if !self.batch.as_ref().is_some_and(|batch| {
            batch.owner == owner && matches!(batch.operation, Operation::Minimized { .. })
        }) {
            return false;
        }
        self.batch = None;
        for placed in &mut self.order {
            if placed.visibility == Visibility::BatchMinimized {
                placed.visibility = Visibility::Visible;
            }
        }
        true
    }

    /// 切换显示桌面；切换决定发生在栈 owner，不读取异步客户端快照。
    pub fn toggle_desktop(&mut self, owner: ClientId) -> bool {
        if self.batch_owner() == Some(owner) && self.showing_desktop() {
            self.undo_minimize_all(owner)
        } else {
            self.begin_batch(owner, true)
        }
    }

    pub(super) fn leave_desktop(&mut self) -> bool {
        if let Some(ShellBatch {
            operation: Operation::Minimized { showing_desktop },
            ..
        }) = &mut self.batch
        {
            std::mem::replace(showing_desktop, false)
        } else {
            false
        }
    }

    /// 排列可见的可缩放根窗口，保留栈序。`area` 是物理工作区，`scale` 是正输出缩放；
    /// `limits` 从当前 xdg owner 投影逻辑尺寸约束，None 排除禁用/模态 owner。无成员返回 false，无错误返回。
    pub fn arrange(
        &mut self,
        owner: ClientId,
        layout: Arrangement,
        area: Geometry,
        scale: i32,
        mut limits: impl FnMut(ClientId, ObjectId) -> Option<SizeLimits>,
    ) -> bool {
        if self.showing_desktop() || scale <= 0 || area.2 < scale || area.3 < scale {
            return false;
        }
        // 1. 固定同一时刻的成员、约束与历史；不改变窗口对象或 Z 序。
        let candidates: Vec<_> = self
            .order
            .iter()
            .filter_map(|placed| {
                if placed.minimized() || placed.parent.is_some() || placed.mode == Mode::Fullscreen
                {
                    return None;
                }
                let (min, max) = limits(placed.client, placed.surface)?;
                if min == max && min.0 > 0 && min.1 > 0 {
                    return None;
                }
                Some((
                    Saved {
                        client: placed.client,
                        surface: placed.surface,
                        geometry: (placed.x, placed.y, placed.width, placed.height),
                        mode: placed.mode,
                        restore: placed.restore,
                    },
                    (min, max),
                ))
            })
            .collect();
        if candidates.is_empty() {
            return false;
        }
        let count = candidates.len();
        let (width, height) = (area.2 / scale, area.3 / scale);
        // 2. 在逻辑像素上划分整数网格，再一次性投影物理像素，避免奇数格被客户端取整后出现缝隙。
        let mut changed = false;
        for (index, (saved, (min, max))) in candidates.iter().enumerate() {
            let (x, y, w, h) = match layout {
                Arrangement::Cascade => (0, 0, width * 3 / 4, height * 3 / 4),
                Arrangement::Horizontal => tile(index, count, width, height),
                Arrangement::Vertical => {
                    let (y, x, h, w) = tile(index, count, height, width);
                    (x, y, w, h)
                }
            };
            let constrain = |size: i32, low: i32, high: i32| {
                size.max(low).max(1).min(if high == 0 {
                    i32::MAX
                } else {
                    high.max(low).max(1)
                })
            };
            let (w, h) = (constrain(w, min.0, max.0), constrain(h, min.1, max.1));
            let (x, y) = if layout == Arrangement::Cascade {
                let slots = ((width - w).min(height - h).max(0) / super::CASCADE_STEP + 1) as usize;
                let offset = (index % slots) as i32 * super::CASCADE_STEP;
                (offset, offset)
            } else {
                (x.min((width - w).max(0)), y.min((height - h).max(0)))
            };
            let placed = self
                .find(saved.client, saved.surface)
                .expect("collected live placement");
            (placed.x, placed.y, placed.width, placed.height) =
                (area.0 + x * scale, area.1 + y * scale, w * scale, h * scale);
            changed |= (placed.x, placed.y, placed.width, placed.height) != saved.geometry
                || saved.mode != Mode::Normal;
            placed.mode = Mode::Normal;
            placed.restore = None;
        }
        if !changed {
            return false;
        }
        for placed in &mut self.order {
            if placed.visibility == Visibility::BatchMinimized {
                placed.visibility = Visibility::Minimized;
            }
        }
        // 3. 只有有效命令替换前次撤销，历史与当前状态的所有权在同一 Stack。
        self.batch = Some(ShellBatch {
            owner,
            operation: Operation::Arranged {
                layout,
                saved: candidates.into_iter().map(|(saved, _)| saved).collect(),
            },
        });
        true
    }

    /// 撤销该连接最近的排列；不改 Z 序，不复活关闭窗口，不覆盖后续手动修改。无匹配批次返回 false。
    pub fn undo_arrangement(&mut self, owner: ClientId) -> bool {
        if self.undo_arrangement_kind(owner).is_none() {
            return false;
        }
        let Some(ShellBatch {
            operation: Operation::Arranged { saved, .. },
            ..
        }) = self.batch.take()
        else {
            unreachable!("checked arrangement");
        };
        for saved in saved {
            if let Some(placed) = self.find(saved.client, saved.surface) {
                (placed.x, placed.y, placed.width, placed.height) = saved.geometry;
                placed.mode = saved.mode;
                placed.restore = saved.restore;
            }
        }
        true
    }

    /// 手动几何/模式/可见性操作接管该窗口，撤销不能覆盖新的用户意图。
    pub fn forget_arranged(&mut self, client: ClientId, surface: ObjectId) {
        if let Some(ShellBatch {
            operation: Operation::Arranged { saved, .. },
            ..
        }) = &mut self.batch
        {
            saved.retain(|saved| (saved.client, saved.surface) != (client, surface));
            if saved.is_empty() {
                self.batch = None;
            }
        }
    }

    pub(super) fn retain_arranged(&mut self) {
        if let Some(ShellBatch {
            operation: Operation::Arranged { saved, .. },
            ..
        }) = &mut self.batch
        {
            saved.retain(|saved| {
                self.order
                    .iter()
                    .any(|placed| (placed.client, placed.surface) == (saved.client, saved.surface))
            });
            if saved.is_empty() {
                self.batch = None;
            }
        }
    }
}

/// 横向优先的近方形网格：每列独立均分余下窗口，不为非平方数量留下空格。
fn tile(index: usize, count: usize, width: i32, height: i32) -> Geometry {
    let mut columns = 1;
    while columns < count / (columns + 1) {
        columns += 1;
    }
    let (mut start, mut column) = (0, 0);
    loop {
        let rows = (count - start).div_ceil(columns - column);
        if index < start + rows {
            let row = index - start;
            let x = (width as i64 * column as i64 / columns as i64) as i32;
            let right = (width as i64 * (column + 1) as i64 / columns as i64) as i32;
            let y = (height as i64 * row as i64 / rows as i64) as i32;
            let bottom = (height as i64 * (row + 1) as i64 / rows as i64) as i32;
            return (x, y, right - x, bottom - y);
        }
        start += rows;
        column += 1;
    }
}
