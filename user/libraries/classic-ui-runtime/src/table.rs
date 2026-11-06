//! 列表几何、表头与滚动共用一个 owner；行只在可见范围读取和绘制。
use super::item_grid::ItemGrid;
use super::widgets::{Node, Tree};
use canvas::Rect;
use classic_ui::{
    ColumnHeader, CursorShape, DividerPress, EventKind, FontRole, Painter, SCROLLBAR_WIDTH, Scroll,
};
use classic_ui_abi::{self as abi, event, widget};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

/// 复选框与标签之间留出可见间距；布局和绘制共用这一宽度。
pub(super) const CHECK_GUTTER: i32 = 17;
#[derive(Clone, Copy)]
pub enum Gesture {
    Header,
    Column(usize),
    Vertical,
    Horizontal,
}
pub struct Table {
    /// 列 ID、标题、应用最后一次指定的宽度。header 独占当前拖动后的有效宽度。
    pub columns: Vec<(u64, String, i32)>,
    pub header: ColumnHeader,
    pub horizontal: Scroll,
    pub gesture: Option<Gesture>,
    pub content_width: i32,
    pub mode: u32,
    pub multiple: bool,
    pub selection: BTreeSet<u64>,
    /// 应用提供的复选框投影。与键盘焦点和列表选区独立。
    pub checks: BTreeMap<u64, u32>,
    pub anchor: Option<u64>,
    pub alignments: BTreeMap<u64, u32>,
    pub last_click: Option<classic_ui::ItemClick<u64>>,
    /// 图标/列表视图的条目格子：（大图标格子，小图标与列表的条目宽度）；布局时由主题图标间距与最宽标签算出。
    pub cells: ((i32, i32), i32),
    /// LVS_EX_FULLROWSELECT：详细资料视图整行高亮；默认只高亮首列标签。
    pub full_row: bool,
}
impl Table {
    pub fn check(&self, row: u64) -> Option<u32> {
        if self.mode == 0 && self.columns.is_empty() {
            self.checks.get(&row).copied()
        } else {
            None
        }
    }

    pub fn check_rect(&self, row: Rect) -> Rect {
        Rect::new(
            row.x - self.horizontal.position + 3,
            row.y + (row.height - 13) / 2,
            13,
            13,
        )
    }
    pub fn extent(&self) -> i32 {
        if self.columns.is_empty() {
            self.content_width
        } else {
            self.header.total()
        }
    }
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            header: ColumnHeader::new([]),
            horizontal: Scroll::default(),
            gesture: None,
            content_width: 0,
            mode: 0,
            multiple: false,
            selection: BTreeSet::new(),
            checks: BTreeMap::new(),
            anchor: None,
            alignments: BTreeMap::new(),
            last_click: None,
            cells: ((75, 75), 200),
            full_row: false,
        }
    }
}
#[derive(Clone, Copy)]
pub struct Geometry {
    pub header: Rect,
    pub rows: Rect,
    pub vertical: Rect,
    pub horizontal: Rect,
    pub shifted_header: Rect,
    pub grid: Option<ItemGrid>,
}
fn contains(r: Rect, p: (i32, i32)) -> bool {
    p.0 >= r.x && p.0 < r.right() && p.1 >= r.y && p.1 < r.bottom()
}
impl Node {
    pub fn list_geometry(&self) -> Geometry {
        let inner = self.rect.inset(self.list_edge());
        let table = self.table.as_ref().expect("list model");
        if table.mode != 0 {
            let cell = if table.mode == 1 {
                table.cells.0
            } else {
                (table.cells.1, self.row_height)
            };
            let mut grid = ItemGrid::new(table.mode, inner, cell, self.row_count());
            let overflow = grid.extent > grid.page();
            let vertical = if overflow && grid.vertical() {
                Rect::new(
                    inner.right() - SCROLLBAR_WIDTH,
                    inner.y,
                    SCROLLBAR_WIDTH,
                    inner.height,
                )
            } else {
                Rect::new(0, 0, 0, 0)
            };
            let horizontal = if overflow && !grid.vertical() {
                Rect::new(
                    inner.x,
                    inner.bottom() - SCROLLBAR_WIDTH,
                    inner.width,
                    SCROLLBAR_WIDTH,
                )
            } else {
                Rect::new(0, 0, 0, 0)
            };
            if overflow {
                let mut viewport = inner;
                if grid.vertical() {
                    viewport.width = (viewport.width - SCROLLBAR_WIDTH).max(0);
                } else {
                    viewport.height = (viewport.height - SCROLLBAR_WIDTH).max(0);
                }
                grid = ItemGrid::new(table.mode, viewport, cell, self.row_count());
            }
            return Geometry {
                header: Rect::new(0, 0, 0, 0),
                rows: grid.viewport,
                vertical,
                horizontal,
                shifted_header: Rect::new(0, 0, 0, 0),
                grid: Some(grid),
            };
        }
        let header_height = if table.columns.is_empty() {
            0
        } else {
            self.row_height + 2
        };
        let mut vertical = false;
        let mut horizontal = false;
        for _ in 0..3 {
            let width = (inner.width - if vertical { SCROLLBAR_WIDTH } else { 0 }).max(0);
            let height =
                (inner.height - header_height - if horizontal { SCROLLBAR_WIDTH } else { 0 })
                    .max(0);
            horizontal = table.extent() > width;
            vertical = self.row_count() as i64 * i64::from(self.row_height) > i64::from(height);
        }
        let width = (inner.width - if vertical { SCROLLBAR_WIDTH } else { 0 }).max(0);
        let height =
            (inner.height - header_height - if horizontal { SCROLLBAR_WIDTH } else { 0 }).max(0);
        let header = Rect::new(inner.x, inner.y, width, header_height);
        let rows = Rect::new(inner.x, inner.y + header_height, width, height);
        Geometry {
            grid: None,
            header,
            rows,
            vertical: Rect::new(
                rows.right(),
                rows.y,
                if vertical { SCROLLBAR_WIDTH } else { 0 },
                rows.height,
            ),
            horizontal: Rect::new(
                rows.x,
                rows.bottom(),
                rows.width,
                if horizontal { SCROLLBAR_WIDTH } else { 0 },
            ),
            shifted_header: Rect::new(
                header.x - table.horizontal.position,
                header.y,
                header.width + table.horizontal.position,
                header.height,
            ),
        }
    }
    pub fn clamp_list(&mut self) {
        let geometry = self.list_geometry();
        if let Some(grid) = geometry.grid {
            self.position.clamp(grid.maximum());
            return;
        }
        let page = (geometry.rows.height / self.row_height).max(1);
        self.position.clamp(self.row_count() as i32 - page);
        let table = self.table.as_mut().expect("list model");
        table.horizontal.clamp(table.extent() - geometry.rows.width);
    }

    fn fit_column(&mut self, column: usize, painter: &Painter<'_>) {
        let table = self.table.as_ref().expect("list model");
        let measure = |text| painter.measure_role(FontRole::Message, text, false);
        let title = measure(&table.columns[column].1);
        let widest = (0..self.row_count())
            .filter_map(|index| self.row_at(index))
            .filter_map(|(id, row)| {
                let text = row.split('\t').nth(column)?;
                let icon = if column == 0 {
                    self.row_icons
                        .get(id)
                        .and_then(|icons| icons.0.as_ref())
                        .map_or(0, |icon| icon.width().max(16) + 4)
                } else {
                    0
                };
                Some(measure(text) + icon)
            })
            .fold(title, i32::max);
        // 表头和普通单元格均在内容两侧留 4 个逻辑像素。
        self.table
            .as_mut()
            .expect("list model")
            .header
            .set_width(column, widest + 8);
    }
}
impl Tree {
    pub fn cursor(&self) -> CursorShape {
        let Some(id) = self.armed.or_else(|| self.hit()) else {
            return CursorShape::Default;
        };
        let node = &self.nodes[&id];
        if node.kind == widget::LINK {
            return CursorShape::Pointer;
        }
        if matches!(node.kind, widget::EDIT | widget::TEXT_EDITOR)
            || node
                .label_edit
                .as_ref()
                .is_some_and(|editing| !editing.pending)
        {
            return CursorShape::Text;
        }
        let Some(table) = node.table.as_ref() else {
            return CursorShape::Default;
        };
        let geometry = node.list_geometry();
        if contains(geometry.header, self.pointer) || matches!(table.gesture, Some(Gesture::Header))
        {
            table
                .header
                .cursor(geometry.shifted_header, self.pointer)
                .unwrap_or(CursorShape::Default)
        } else {
            CursorShape::Default
        }
    }
    pub fn table_input(
        &mut self,
        window: u64,
        input: &EventKind,
        painter: &Painter<'_>,
        out: &mut Vec<abi::Event>,
    ) -> bool {
        let point = match input {
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                (x.floor() as i32, y.floor() as i32)
            }
            _ => self.pointer,
        };
        let target = self.armed.or_else(|| self.hit());
        let Some(id) = target.filter(|id| {
            self.nodes
                .get(id)
                .is_some_and(|node| matches!(node.kind, widget::LIST | widget::TREE))
                && self.active(*id)
        }) else {
            return false;
        };
        let node = self.nodes.get_mut(&id).expect("list target");
        let geometry = node.list_geometry();
        let rows = geometry
            .grid
            .map_or_else(|| node.row_count() as i32, |grid| grid.extent);
        let page = geometry.grid.map_or_else(
            || (geometry.rows.height / node.row_height).max(1),
            |grid| grid.page(),
        );
        let step = geometry
            .grid
            .map_or(1, |grid| if grid.vertical() { grid.height } else { 16 });
        let table = node.table.as_mut().expect("table model");
        let mut fit_column = None;
        match input {
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } => {
                let on_header = contains(geometry.header, point);
                if !on_header {
                    table.header.cancel();
                }
                let gesture = if on_header {
                    match table
                        .header
                        .press(geometry.shifted_header, point, Instant::now())
                    {
                        DividerPress::Drag => Some(Gesture::Header),
                        DividerPress::Fit(index) => {
                            fit_column = Some(index);
                            Some(Gesture::Header)
                        }
                        DividerPress::Outside => table
                            .header
                            .column_at(geometry.shifted_header, point)
                            .map(Gesture::Column),
                    }
                } else if contains(geometry.vertical, point) {
                    node.position
                        .press(geometry.vertical, true, (rows, page, step), point);
                    Some(Gesture::Vertical)
                } else if contains(geometry.horizontal, point) {
                    if geometry.grid.is_some() {
                        node.position
                            .press(geometry.horizontal, false, (rows, page, step), point);
                    } else {
                        table.horizontal.press(
                            geometry.horizontal,
                            false,
                            (table.extent(), geometry.rows.width, 24),
                            point,
                        );
                    }
                    Some(Gesture::Horizontal)
                } else {
                    None
                };
                if gesture.is_none() {
                    return false;
                }
                table.gesture = gesture;
                self.focus = Some(id);
                self.armed = Some(id);
            }
            EventKind::PointerEnter { .. } | EventKind::PointerMove { .. } => {
                self.pointer = point;
                match table.gesture {
                    Some(Gesture::Header) => {
                        table.header.motion(point);
                    }
                    Some(Gesture::Vertical) => {
                        node.position
                            .drag(geometry.vertical, true, (rows, page), point)
                    }
                    Some(Gesture::Horizontal) => {
                        if geometry.grid.is_some() {
                            node.position
                                .drag(geometry.horizontal, false, (rows, page), point);
                        } else {
                            table.horizontal.drag(
                                geometry.horizontal,
                                false,
                                (table.extent(), geometry.rows.width),
                                point,
                            );
                        }
                    }
                    Some(Gesture::Column(_)) => {}
                    None => return false,
                }
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            } => {
                let Some(gesture) = table.gesture.take() else {
                    return false;
                };
                if let Gesture::Column(index) = gesture
                    && contains(geometry.header, point)
                    && table.header.column_at(geometry.shifted_header, point) == Some(index)
                {
                    out.push(abi::Event {
                        window,
                        node: id,
                        kind: event::COLUMN,
                        value: table.columns[index].0,
                        ..Default::default()
                    });
                }
                table.header.release(point);
                table.horizontal.release();
                node.position.release();
                self.armed = None;
            }
            EventKind::PointerLeave | EventKind::Focus(false) => {
                table.header.cancel();
                if table.gesture.take().is_none() {
                    return false;
                }
                table.horizontal.release();
                node.position.release();
                self.armed = None;
            }
            EventKind::Wheel {
                vertical,
                horizontal,
            } => {
                if let Some(grid) = geometry.grid {
                    let notches = if grid.vertical() {
                        *vertical
                    } else if *horizontal != 0 {
                        *horizontal
                    } else {
                        *vertical
                    };
                    node.position.wheel(
                        notches,
                        if grid.vertical() { grid.height } else { 16 },
                        grid.maximum(),
                    );
                } else {
                    node.position.wheel(*vertical, 1, (rows - page).max(0));
                    table
                        .horizontal
                        .wheel(*horizontal, 24, table.extent() - geometry.rows.width);
                }
            }
            _ => return false,
        }
        if let Some(column) = fit_column {
            node.fit_column(column, painter);
        }
        node.clamp_list();
        true
    }
}

impl Node {
    /// 滚动列表，让索引 `index` 的条目进入当前视口。
    pub fn reveal_list_row(&mut self, index: usize, geometry: Geometry) {
        let scroll = self.position.position;
        self.position.position = if let Some(grid) = geometry.grid {
            grid.reveal(index, scroll)
        } else {
            let page = (geometry.rows.height / self.row_height).max(1);
            scroll.min(index as i32).max(index as i32 + 1 - page)
        };
    }

    pub fn row_selected(&self, id: u64) -> bool {
        self.table
            .as_ref()
            .filter(|table| table.multiple)
            .map_or_else(
                || self.selected == Some(id),
                |table| table.selection.contains(&id),
            )
    }
    pub fn choose_row(
        &mut self,
        index: usize,
        shift: bool,
        control: bool,
        toggle: bool,
    ) -> Option<u64> {
        let value = self.row_at(index)?.0;
        let table = self.table.as_ref()?;
        let anchor = table.anchor.or(self.selected).unwrap_or(value);
        let start = (0..self.row_count())
            .find(|index| self.row_at(*index).is_some_and(|row| row.0 == anchor))
            .unwrap_or(index);
        let range: Vec<_> = if shift && table.multiple {
            (start.min(index)..=start.max(index))
                .filter_map(|index| self.row_at(index).map(|row| row.0))
                .collect()
        } else {
            Vec::new()
        };
        let table = self.table.as_mut().expect("list model");
        if table.multiple {
            if shift {
                if !control {
                    table.selection.clear();
                }
                table.selection.extend(range);
                table.anchor = Some(anchor);
            } else if control {
                if toggle {
                    if !table.selection.remove(&value) {
                        table.selection.insert(value);
                    }
                    table.anchor = Some(value);
                }
            } else {
                table.selection.clear();
                table.selection.insert(value);
                table.anchor = Some(value);
            }
        }
        self.selected = Some(value);
        Some(value)
    }
}
