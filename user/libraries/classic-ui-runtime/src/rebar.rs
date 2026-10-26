//! comctl32 ReBar（RBS_BANDBORDERS，无 CCS_NODIVIDER）：每个子控件是一行带区，
//! 左侧把手、带区之间的蚀刻边线和右上角的品牌 logo 由运行库绘制；带区几何在布局中一次算出。
use super::widgets::Tree;
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::Painter;
use std::{rc::Rc, time::Duration, time::Instant};

/// 带区内容的左偏移：边距 2 + 把手 3 + 把手后间距 4（comctl32 REBAR_PRE_GRIPPER / GRIPPER_WIDTH / REBAR_POST_GRIPPER）。
const PRE_GRIPPER: i32 = 2;
const GRIPPER: i32 = 3;
const POST_GRIPPER: i32 = 4;
/// 带区上下内边距。
const BAND_PAD: i32 = 2;
/// 带区边线与顶部分隔线的高度（蚀刻：阴影 + 高光）。
const DIVIDER: i32 = 2;
/// 品牌动画帧间隔。
const FRAME_INTERVAL: Duration = Duration::from_millis(70);

/// 品牌 logo：帧竖排的位图条，帧为正方形；22/26/38 像素三种尺寸按第一行高度选择。
#[derive(Default)]
pub struct Rebar {
    brands: Vec<Rc<classic_ui::Bitmap>>,
    pub rows: Vec<Rect>,
    brand: Option<Rect>,
    frame: i32,
    /// 忙碌时下一帧的时刻；None 表示静止在首帧。
    next_frame: Option<Instant>,
}

impl Rebar {
    pub fn new(brands: Vec<Rc<classic_ui::Bitmap>>) -> Self {
        Self {
            brands,
            ..Default::default()
        }
    }

    /// 内容相对带区的偏移：(左, 上, 右, 下)。
    pub fn content_inset() -> (i32, i32, i32, i32) {
        (
            PRE_GRIPPER + GRIPPER + POST_GRIPPER,
            BAND_PAD,
            PRE_GRIPPER,
            BAND_PAD,
        )
    }

    /// 由带区内容高度得到总高：顶部分隔线 + 各行（内容 + 上下内边距）+ 行间边线。
    pub fn height(contents: &[i32]) -> i32 {
        let rows: i32 = contents.iter().map(|height| height + 2 * BAND_PAD).sum();
        DIVIDER + rows + DIVIDER * (contents.len() as i32 - 1).max(0)
    }

    /// 品牌 logo 的边长：不超过第一行高度的最大可用尺寸。
    pub fn brand_size(&self, first_row: i32) -> Option<i32> {
        self.brands
            .iter()
            .map(|brand| brand.width())
            .filter(|size| *size <= first_row)
            .max()
    }

    /// 记录布局：各行矩形与品牌区域；返回各行内容矩形（第一行在品牌左侧收窄）。
    pub fn layout(&mut self, rect: Rect, contents: &[i32]) -> Vec<Rect> {
        let (left, top, right, bottom) = Self::content_inset();
        let mut y = rect.y + DIVIDER;
        self.rows.clear();
        let mut areas = Vec::with_capacity(contents.len());
        for (index, content) in contents.iter().enumerate() {
            let row = Rect::new(rect.x, y, rect.width, content + 2 * BAND_PAD);
            let mut area = Rect::new(
                row.x + left,
                row.y + top,
                (row.width - left - right).max(0),
                (row.height - top - bottom).max(0),
            );
            if index == 0 {
                self.brand = self.brand_size(row.height).map(|size| {
                    Rect::new(
                        row.right() - size,
                        row.y + (row.height - size) / 2,
                        size,
                        size,
                    )
                });
                if let Some(brand) = self.brand {
                    area.width = (brand.x - area.x - right).max(0);
                }
            }
            self.rows.push(row);
            areas.push(area);
            y = row.bottom() + DIVIDER;
        }
        areas
    }

    pub fn paint(&self, p: &mut Painter<'_>, rect: Rect) {
        p.fill(rect, C::ButtonFace);
        etched(p, Rect::new(rect.x, rect.y, rect.width, DIVIDER));
        for (index, row) in self.rows.iter().enumerate() {
            p.edge(
                Rect::new(
                    row.x + PRE_GRIPPER,
                    row.y + BAND_PAD,
                    GRIPPER,
                    row.height - 2 * BAND_PAD,
                ),
                Edge::RaisedInner,
            );
            if index + 1 < self.rows.len() {
                etched(p, Rect::new(row.x, row.bottom(), row.width, DIVIDER));
            }
        }
        if let Some(area) = self.brand
            && let Some(strip) = self.brands.iter().find(|brand| brand.width() == area.width)
        {
            let frames = (strip.height() / area.width).max(1);
            strip.paint_rows(
                p,
                area.x,
                area.y,
                (self.frame % frames) * area.width,
                area.width,
            );
        }
    }

    /// 设置忙碌状态；空闲回到首帧。
    pub fn set_busy(&mut self, busy: bool) {
        if busy {
            self.next_frame
                .get_or_insert_with(|| Instant::now() + FRAME_INTERVAL);
        } else {
            self.next_frame = None;
            self.frame = 0;
        }
    }

    /// 下一帧的截止时刻。
    pub fn deadline(&self) -> Option<Instant> {
        self.next_frame
    }

    /// 到期则推进一帧（首帧是静止画面，动画在其余帧间循环）；返回是否需要重绘。
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some(due) = self.next_frame.filter(|due| *due <= now) else {
            return false;
        };
        let frames = self
            .brands
            .first()
            .map_or(1, |brand| (brand.height() / brand.width().max(1)).max(1));
        self.frame = if frames > 1 {
            self.frame % (frames - 1) + 1
        } else {
            0
        };
        self.next_frame = Some(due + FRAME_INTERVAL);
        true
    }
}

fn etched(p: &mut Painter<'_>, rect: Rect) {
    p.fill(Rect::new(rect.x, rect.y, rect.width, 1), C::ButtonShadow);
    p.fill(
        Rect::new(rect.x, rect.y + 1, rect.width, 1),
        C::ButtonHilight,
    );
}

impl Tree {
    /// 最早的定时截止时刻：ReBar 品牌动画、菜单的子菜单展开延时与工具栏提示延时。
    pub fn animation_deadline(&self) -> Option<Instant> {
        let now = Instant::now();
        self.nodes
            .values()
            .filter_map(|node| {
                let rebar = node.rebar.as_ref().and_then(Rebar::deadline);
                let menu = node.menu.as_ref().and_then(|menu| menu.state.deadline());
                let tip = node
                    .toolbar
                    .as_ref()
                    .and_then(|toolbar| toolbar.deadline(now));
                rebar.into_iter().chain(menu).chain(tip).min()
            })
            .min()
    }

    /// 推进到期的品牌动画、展开到期的子菜单并显示到期的工具提示；返回是否需要重绘。
    pub fn animate(&mut self, now: Instant) -> bool {
        let last = std::mem::replace(&mut self.animated, now);
        let mut repaint = false;
        for node in self.nodes.values_mut() {
            if let Some(rebar) = node.rebar.as_mut() {
                repaint |= rebar.tick(now);
            }
            if let Some(menu) = node.menu.as_mut() {
                repaint |= menu.state.tick(now);
            }
            if let Some(toolbar) = node.toolbar.as_mut() {
                repaint |= toolbar.tick(now, last);
            }
        }
        repaint
    }
}
