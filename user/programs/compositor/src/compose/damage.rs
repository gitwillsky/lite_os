//! 输出损伤、缓冲年龄与可见区域。只保留几何，不保活客户端像素或 dma-buf 使用期。
use super::Drawable;
use canvas::{ImageSource, Rect};
use classic_theme::Rgb;

/// 限制碎片化的 CPU/命令开销；损伤溢出扩大范围，遮挡溢出放弃本次减法，均只会多画。
const RECT_LIMIT: usize = 128;

/// 不相交矩形的并集；透明混合不能把重叠损伤绘制两次。
#[derive(Clone, Default)]
pub(super) struct Region {
    rects: Vec<Rect>,
}
impl Region {
    pub(super) fn of(rect: Rect) -> Self {
        let mut region = Self::default();
        region.add(rect);
        region
    }
    pub(super) fn rects(&self) -> &[Rect] {
        &self.rects
    }
    pub(super) fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }
    pub(super) fn clipped(&self, bounds: Rect) -> Self {
        Self {
            rects: self
                .rects
                .iter()
                .map(|rect| rect.intersect(bounds))
                .filter(|rect| !rect.is_empty())
                .collect(),
        }
    }
    pub(super) fn add(&mut self, rect: Rect) {
        if rect.is_empty()
            || self
                .rects
                .iter()
                .any(|existing| existing.intersect(rect) == rect)
        {
            return;
        }
        if !self.cut(rect, 1) {
            self.collapse(rect);
            return;
        }
        self.rects.push(rect);
    }
    pub(super) fn subtract(&mut self, rect: Rect) {
        self.cut(rect, 0);
    }
    /// 先证明容量再原地分割；逆序处理旧成员，swap_remove 只会移来已处理的片段。
    fn cut(&mut self, rect: Rect, additional: usize) -> bool {
        let required = self
            .rects
            .iter()
            .map(|part| {
                split(*part, rect)
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .count()
            })
            .sum::<usize>()
            + additional;
        if required > RECT_LIMIT {
            return false;
        }
        self.rects
            .reserve(required.saturating_sub(self.rects.len()));
        for index in (0..self.rects.len()).rev() {
            let mut parts = split(self.rects[index], rect)
                .into_iter()
                .filter(|part| !part.is_empty());
            match parts.next() {
                Some(first) => {
                    self.rects[index] = first;
                    self.rects.extend(parts);
                }
                None => {
                    self.rects.swap_remove(index);
                }
            }
        }
        true
    }
    fn collapse(&mut self, additional: Rect) {
        let bounds = self.rects.iter().fold(additional, |bounds, rect| {
            let (x, y) = (bounds.x.min(rect.x), bounds.y.min(rect.y));
            Rect::new(
                x,
                y,
                bounds.right().max(rect.right()).saturating_sub(x),
                bounds.bottom().max(rect.bottom()).saturating_sub(y),
            )
        });
        self.rects.clear();
        self.rects.push(bounds);
    }
}
fn split(rect: Rect, cut: Rect) -> [Rect; 4] {
    let overlap = rect.intersect(cut);
    if overlap.is_empty() {
        return [rect, Rect::default(), Rect::default(), Rect::default()];
    }
    [
        Rect::new(rect.x, rect.y, rect.width, overlap.y - rect.y),
        Rect::new(
            rect.x,
            overlap.bottom(),
            rect.width,
            rect.bottom() - overlap.bottom(),
        ),
        Rect::new(rect.x, overlap.y, overlap.x - rect.x, overlap.height),
        Rect::new(
            overlap.right(),
            overlap.y,
            rect.right() - overlap.right(),
            overlap.height,
        ),
    ]
}

#[derive(Clone, PartialEq)]
struct Snapshot {
    identity: (u64, u32),
    order: usize,
    rect: Rect,
    source: ImageSource,
}
impl Snapshot {
    fn of(window: &Drawable<'_>, order: usize) -> Self {
        Self {
            identity: window
                .identity
                .expect("presented drawable has no surface identity"),
            order,
            rect: window.rect,
            source: window.content.source,
        }
    }
}

/// 单一输出 owner 的元数据；两个目标各自的有效性与上一帧损伤足以修复双缓冲的年龄。
#[derive(Default)]
pub(crate) struct History {
    scene: Vec<Snapshot>,
    last_damage: Region,
    valid: [bool; 2],
    background: Option<Rgb>,
    overlay: Option<Rect>,
}
/// 一次输出合成的已裁剪工作；按原堆叠顺序绘制 windows，再绘制 overlay。
pub(crate) struct Frame {
    scene: Vec<Snapshot>,
    damage: Region,
    /// 目标缓冲相对其上次内容的全部修改，包含缓冲年龄修复；不是仅相对前台的 damage。
    repair: Region,
    pub(super) background: Region,
    pub(super) windows: Vec<Region>,
    pub(super) overlay: Region,
    color: Rgb,
    overlay_bounds: Option<Rect>,
}
impl Frame {
    /// 相对已呈现前台的输出差异，用于标准 KMS FB_DAMAGE_CLIPS。
    pub(crate) fn output_rects(&self) -> &[Rect] {
        self.damage.rects()
    }
    /// 本次目标缓冲修改的物理矩形，用于同步该缓冲的设备副本。
    pub(crate) fn repair_rects(&self) -> &[Rect] {
        self.repair.rects()
    }
}

impl History {
    pub(crate) fn invalidate(&mut self) {
        *self = Self::default();
    }
    pub(crate) fn prepare(
        &mut self,
        back: usize,
        screen: Rect,
        color: Rgb,
        windows: &[Drawable<'_>],
        overlay: Option<Rect>,
    ) -> Frame {
        let mut scene: Vec<_> = windows
            .iter()
            .enumerate()
            .map(|(index, window)| Snapshot::of(window, index))
            .collect();
        scene.sort_unstable_by_key(|entry| entry.identity);
        let mut damage = Region::default();
        for old in &self.scene {
            if scene
                .binary_search_by_key(&old.identity, |entry| entry.identity)
                .ok()
                .is_none_or(|index| scene[index] != *old)
            {
                damage.add(old.rect.intersect(screen));
            }
        }
        for (index, window) in windows.iter().enumerate() {
            let next = Snapshot::of(window, index);
            let unchanged = self
                .scene
                .binary_search_by_key(&next.identity, |entry| entry.identity)
                .ok()
                .is_some_and(|index| self.scene[index] == next);
            if unchanged {
                for rect in window.damage {
                    damage
                        .add(project(*rect, window.content.source, window.rect).intersect(screen));
                }
            } else {
                damage.add(window.rect.intersect(screen));
            }
        }
        // 覆盖层没有客户端 commit，内容由 Session 状态决定；保留旧位置以擦除菜单/切换框。
        for rect in self.overlay.into_iter().chain(overlay) {
            damage.add(rect.intersect(screen));
        }
        if self.background != Some(color) {
            damage = Region::of(screen);
        }
        let mut repair = if self.valid[back] {
            damage.clone()
        } else {
            Region::of(screen)
        };
        if self.valid[back] {
            for rect in self.last_damage.rects() {
                repair.add(*rect);
            }
        }
        // 从上向下扣除已证明不透明的覆盖，只跳过不再可能贡献像素的部分。
        let overlay_region = overlay.map_or_else(Region::default, |bounds| repair.clipped(bounds));
        let mut uncovered = repair.clone();
        let mut visible = Vec::with_capacity(windows.len());
        for window in windows.iter().rev() {
            visible.push(uncovered.clipped(window.rect));
            if window.opaque {
                uncovered.subtract(window.rect);
            }
        }
        visible.reverse();
        // 绘制失败后这块目标不能再按旧年龄复用；只有成功才重新标有效。
        self.valid[back] = false;
        Frame {
            scene,
            damage,
            repair,
            background: uncovered,
            windows: visible,
            overlay: overlay_region,
            color,
            overlay_bounds: overlay,
        }
    }
    pub(crate) fn record(&mut self, back: usize, frame: Frame) {
        self.scene = frame.scene;
        self.last_damage = frame.damage;
        self.background = Some(frame.color);
        self.overlay = frame.overlay_bounds;
        self.valid[back] = true;
    }
}

/// buffer 像素损伤经仿射采样的逆变换投影到输出；向外取整保住旋转/分数 viewport 的边缘。
fn project(damage: Rect, source: ImageSource, dest: Rect) -> Rect {
    let a = (
        source.right.0 - source.origin.0,
        source.right.1 - source.origin.1,
    );
    let b = (
        source.bottom.0 - source.origin.0,
        source.bottom.1 - source.origin.1,
    );
    let determinant = a.0 * b.1 - a.1 * b.0;
    if !determinant.is_finite() || determinant == 0.0 {
        return dest;
    }
    let mut left = f64::INFINITY;
    let mut top = f64::INFINITY;
    let mut right = f64::NEG_INFINITY;
    let mut bottom = f64::NEG_INFINITY;
    for (x, y) in [
        (damage.x, damage.y),
        (damage.right(), damage.y),
        (damage.x, damage.bottom()),
        (damage.right(), damage.bottom()),
    ] {
        let x = f64::from(x) - source.origin.0;
        let y = f64::from(y) - source.origin.1;
        let u = (x * b.1 - y * b.0) / determinant;
        let v = (a.0 * y - a.1 * x) / determinant;
        let x = f64::from(dest.x) + u * f64::from(dest.width);
        let y = f64::from(dest.y) + v * f64::from(dest.height);
        left = left.min(x);
        top = top.min(y);
        right = right.max(x);
        bottom = bottom.max(y);
    }
    let left = (left.floor() as i32).saturating_sub(1);
    let top = (top.floor() as i32).saturating_sub(1);
    let right = (right.ceil() as i32).saturating_add(1);
    let bottom = (bottom.ceil() as i32).saturating_add(1);
    Rect::new(
        left,
        top,
        right.saturating_sub(left),
        bottom.saturating_sub(top),
    )
    .intersect(dest)
}
