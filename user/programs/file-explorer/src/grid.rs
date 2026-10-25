//! 文件列表的四种查看方式的几何与滚动条交互；所有数值都是逻辑像素。
use canvas::Rect;
use classic_ui::SCROLLBAR_WIDTH;

/// 列表的查看方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    LargeIcons,
    SmallIcons,
    List,
    Details,
}

/// "详细信息"表头高度。
pub const HEADER_HEIGHT: i32 = 17;
/// "详细信息"各列宽度：名称、大小、类型、修改时间。
pub const COLUMNS: [i32; 4] = [150, 64, 96, 126];

pub fn inside(rect: Rect, (x, y): (i32, i32)) -> bool {
    (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y)
}

/// 一次布局的结果：可见区、滚动条位置与内容总长。
///
/// 大/小图标自左向右换行、纵向滚动；列表按列自上向下排、横向滚动；详细信息一行一项、纵向滚动。
pub struct Grid {
    pub view: ViewMode,
    /// 项目绘制区（不含滚动条）。
    pub viewport: Rect,
    /// 需要滚动时的滚动条矩形。
    pub bar: Option<Rect>,
    /// 沿滚动轴的内容总长。
    pub extent: i32,
    count: usize,
}

impl Grid {
    fn cell_size(view: ViewMode, width: i32) -> (i32, i32) {
        match view {
            ViewMode::LargeIcons => (75, 64),
            ViewMode::SmallIcons => (150, 18),
            ViewMode::List => (170, 18),
            ViewMode::Details => (width, 17),
        }
    }

    fn header(view: ViewMode) -> i32 {
        if view == ViewMode::Details {
            HEADER_HEIGHT
        } else {
            0
        }
    }

    pub fn vertical(&self) -> bool {
        self.view != ViewMode::List
    }

    /// 在 `area` 内布局 `count` 个项目；内容放不下时让出滚动条所占的一条边再排一次。
    pub fn new(view: ViewMode, area: Rect, count: usize) -> Self {
        let measure = |viewport: Rect| {
            let (cell_width, cell_height) = Self::cell_size(view, viewport.width);
            let page = viewport.height - Self::header(view);
            match view {
                ViewMode::List => {
                    let rows = (page / cell_height).max(1);
                    (count as i32 + rows - 1) / rows * cell_width
                }
                _ => {
                    let columns = (viewport.width / cell_width).max(1);
                    (count as i32 + columns - 1) / columns * cell_height
                }
            }
        };
        let overflows = |viewport: Rect, extent: i32| {
            extent
                > if view == ViewMode::List {
                    viewport.width
                } else {
                    viewport.height - Self::header(view)
                }
        };
        let mut viewport = area;
        let mut extent = measure(viewport);
        let mut bar = None;
        if overflows(viewport, extent) {
            if view == ViewMode::List {
                bar = Some(Rect::new(
                    area.x,
                    area.bottom() - SCROLLBAR_WIDTH,
                    area.width,
                    SCROLLBAR_WIDTH,
                ));
                viewport.height -= SCROLLBAR_WIDTH;
            } else {
                bar = Some(Rect::new(
                    area.right() - SCROLLBAR_WIDTH,
                    area.y,
                    SCROLLBAR_WIDTH,
                    area.height,
                ));
                viewport.width -= SCROLLBAR_WIDTH;
            }
            extent = measure(viewport);
        }
        Self {
            view,
            viewport,
            bar,
            extent,
            count,
        }
    }

    /// 滚动轴方向的可见长度。
    pub fn page(&self) -> i32 {
        if self.vertical() {
            self.viewport.height - Self::header(self.view)
        } else {
            self.viewport.width
        }
    }

    /// 单击滚动箭头移动的距离。
    pub fn line(&self) -> i32 {
        Self::cell_size(self.view, 0).1
    }

    pub fn max_scroll(&self) -> i32 {
        (self.extent - self.page()).max(0)
    }

    /// 第 `index` 项在滚动 `scroll` 后的单元格矩形。
    pub fn cell(&self, index: usize, scroll: i32) -> Rect {
        let (cell_width, cell_height) = Self::cell_size(self.view, self.viewport.width);
        let top = self.viewport.y + Self::header(self.view);
        let index = index as i32;
        match self.view {
            ViewMode::List => {
                let rows = (self.page_rows()).max(1);
                Rect::new(
                    self.viewport.x + index / rows * cell_width - scroll,
                    top + index % rows * cell_height,
                    cell_width,
                    cell_height,
                )
            }
            ViewMode::Details => Rect::new(
                self.viewport.x,
                top + index * cell_height - scroll,
                cell_width,
                cell_height,
            ),
            _ => {
                let columns = (self.viewport.width / cell_width).max(1);
                Rect::new(
                    self.viewport.x + index % columns * cell_width,
                    top + index / columns * cell_height - scroll,
                    cell_width,
                    cell_height,
                )
            }
        }
    }

    fn page_rows(&self) -> i32 {
        (self.viewport.height / Self::cell_size(self.view, 0).1).max(1)
    }

    /// 命中的项目序号。
    pub fn at(&self, point: (i32, i32), scroll: i32) -> Option<usize> {
        let items = Rect::new(
            self.viewport.x,
            self.viewport.y + Self::header(self.view),
            self.viewport.width,
            self.page_height(),
        );
        if !inside(items, point) {
            return None;
        }
        (0..self.count).find(|&i| inside(self.cell(i, scroll), point))
    }

    fn page_height(&self) -> i32 {
        self.viewport.height - Self::header(self.view)
    }

    /// 返回使第 `index` 项完全可见的最小滚动位置调整结果。
    pub fn reveal(&self, index: usize, scroll: i32) -> i32 {
        let cell = self.cell(index, scroll);
        let (start, end, low, high) = if self.vertical() {
            (
                cell.y,
                cell.bottom(),
                self.viewport.y + Self::header(self.view),
                self.viewport.bottom(),
            )
        } else {
            (cell.x, cell.right(), self.viewport.x, self.viewport.right())
        };
        let moved = if start < low {
            scroll - (low - start)
        } else if end > high {
            scroll + (end - high)
        } else {
            scroll
        };
        moved.clamp(0, self.max_scroll())
    }

    /// 方向键移动选择：大/小图标按行列、列表按列、详细信息按行；越界时停在端点。
    pub fn step(&self, from: usize, dx: i32, dy: i32) -> usize {
        let delta: isize = match self.view {
            ViewMode::Details => dy as isize,
            ViewMode::List => dy as isize + dx as isize * self.page_rows() as isize,
            ViewMode::LargeIcons | ViewMode::SmallIcons => {
                let columns = (self.viewport.width / Self::cell_size(self.view, 0).0).max(1);
                (dy * columns + dx) as isize
            }
        };
        (from as isize + delta).clamp(0, self.count as isize - 1) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use classic_ui::Scroll;

    fn area() -> Rect {
        Rect::new(0, 0, 300, 100)
    }

    #[test]
    fn content_that_fits_has_no_scrollbar_and_overflow_takes_the_edge() {
        let fits = Grid::new(ViewMode::Details, area(), 3);
        assert!(fits.bar.is_none());
        assert_eq!(fits.viewport, area());
        let overflow = Grid::new(ViewMode::Details, area(), 50);
        assert_eq!(overflow.viewport.width, 300 - SCROLLBAR_WIDTH);
        assert_eq!(overflow.max_scroll(), 50 * 17 - (100 - HEADER_HEIGHT));
        let list = Grid::new(ViewMode::List, area(), 50);
        assert_eq!(list.viewport.height, 100 - SCROLLBAR_WIDTH);
        assert!(!list.vertical());
    }

    #[test]
    fn icon_views_wrap_by_width_and_list_flows_down_then_across() {
        let large = Grid::new(ViewMode::LargeIcons, Rect::new(0, 0, 300, 300), 10);
        assert_eq!(large.cell(0, 0), Rect::new(0, 0, 75, 64));
        assert_eq!(large.cell(4, 0).x, 0);
        assert_eq!(large.cell(4, 0).y, 64);
        let list = Grid::new(ViewMode::List, area(), 20);
        // 20 项横向溢出，横向滚动条占去 16 像素高度，所以每列 (100 - 16) / 18 = 4 行。
        let rows = (100 - SCROLLBAR_WIDTH) / 18;
        assert_eq!(list.cell(rows as usize, 0).x, 170);
        assert_eq!(list.cell(rows as usize, 0).y, 0);
    }

    #[test]
    fn hit_testing_follows_scroll_and_ignores_the_header() {
        let grid = Grid::new(ViewMode::Details, area(), 50);
        assert_eq!(grid.at((10, HEADER_HEIGHT + 1), 0), Some(0));
        assert_eq!(grid.at((10, HEADER_HEIGHT + 1), 34), Some(2));
        assert_eq!(grid.at((10, 5), 0), None);
    }

    #[test]
    fn reveal_scrolls_only_as_far_as_needed() {
        let grid = Grid::new(ViewMode::Details, area(), 50);
        assert_eq!(grid.reveal(0, 0), 0);
        let last_visible = ((100 - HEADER_HEIGHT) / 17) as usize;
        assert!(grid.reveal(last_visible, 0) > 0);
        assert_eq!(grid.reveal(49, 0), grid.max_scroll());
    }

    #[test]
    fn arrow_keys_stop_at_the_ends() {
        let grid = Grid::new(ViewMode::Details, area(), 5);
        assert_eq!(grid.step(0, 0, -1), 0);
        assert_eq!(grid.step(4, 0, 1), 4);
        assert_eq!(grid.step(1, 0, 1), 2);
        let large = Grid::new(ViewMode::LargeIcons, Rect::new(0, 0, 300, 300), 10);
        assert_eq!(large.step(1, 0, 1), 5);
    }

    #[test]
    fn scrollbar_arrows_tracks_and_thumb_drag_move_the_position() {
        let bar = Rect::new(0, 0, SCROLLBAR_WIDTH, 116);
        let (content, page) = (1000, 100);
        let mut scroll = Scroll::default();
        scroll.press(bar, true, (content, page, 17), (5, 4));
        assert_eq!(scroll.position, 0, "up arrow clamps at the top");
        scroll.press(bar, true, (content, page, 17), (5, 112));
        assert_eq!(scroll.position, 17);
        scroll.press(bar, true, (content, page, 17), (5, 90));
        assert_eq!(scroll.position, 117, "track below the thumb pages down");
        scroll.position = 0;
        scroll.press(bar, true, (content, page, 17), (5, 18));
        assert!(scroll.pressed().is_some());
        scroll.drag(bar, true, (content, page), (5, 200));
        assert_eq!(scroll.position, content - page);
        scroll.release();
        scroll.drag(bar, true, (content, page), (5, 0));
        assert_eq!(scroll.position, content - page, "no drag after release");
    }
}
