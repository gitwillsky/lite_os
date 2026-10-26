//! 同一份逻辑矩形用于绘制、裁剪与命中；剩余像素按累计权重分配，不产生缩放缝隙。
use super::widgets::Tree;
use canvas::Rect;
use classic_ui::Painter;
use classic_ui_abi::{state, widget};

impl Tree {
    pub fn layout(&mut self, bounds: Rect, font: &Painter<'_>) {
        let scale = font.scale();
        if !self.layout_dirty && self.layout_bounds == Some((bounds, scale)) {
            return;
        }
        self.layout_dirty = false;
        self.layout_bounds = Some((bounds, scale));
        for node in self.nodes.values_mut() {
            node.clip = Rect::new(0, 0, 0, 0);
        }
        let roots = self.roots();
        self.place_children(&roots, bounds, bounds, false, 0, font);
        if !self
            .focus
            .is_some_and(|id| self.active(id) && self.nodes[&id].focusable())
        {
            self.focus = None;
            self.focus_next(false);
        }
    }
    fn natural(&self, id: u64, available_width: i32, font: &Painter<'_>, scale: i32) -> (i32, i32) {
        let node = &self.nodes[&id];
        // 标签/链接可指定派生字体：度量必须与绘制使用同一份解析结果。
        let custom = node
            .logical_font(&font.theme().fonts)
            .map(|logical| font.fonts().custom(&logical));
        let line = custom.as_ref().map_or_else(
            || font.line_height(),
            |face| {
                let (ascent, descent) = face.metrics();
                (ascent + descent + scale - 1) / scale
            },
        );
        let measure = |text: &str| {
            custom.as_ref().map_or_else(
                || font.text_width(text, false),
                |face| face.measure(text, false),
            )
        };
        let width = (measure(node.text()) + scale - 1) / scale + 12;
        let children: Vec<_> = node
            .children
            .iter()
            .filter(|id| self.nodes[id].visible())
            .copied()
            .collect();
        let natural = match node.kind {
            widget::ROW | widget::COLUMN | widget::GROUP => {
                let sizes: Vec<_> = children
                    .iter()
                    .map(|id| {
                        self.natural(
                            *id,
                            (available_width - node.layout.padding * 2).max(0),
                            font,
                            scale,
                        )
                    })
                    .collect();
                let gap = node
                    .layout
                    .gap
                    .saturating_mul(sizes.len().saturating_sub(1) as i32);
                if node.kind == widget::ROW {
                    (
                        sizes.iter().map(|v| v.0).sum::<i32>().saturating_add(gap),
                        sizes.iter().map(|v| v.1).max().unwrap_or(0),
                    )
                } else {
                    (
                        sizes.iter().map(|v| v.0).max().unwrap_or(0),
                        sizes.iter().map(|v| v.1).sum::<i32>().saturating_add(gap),
                    )
                }
            }
            widget::LABEL if node.state & state::WRAP != 0 => {
                let width = if node.layout.width >= 0 {
                    node.layout.width
                } else {
                    available_width
                };
                let inner = (width - node.layout.padding * 2).max(0);
                let height = match &custom {
                    Some(face) => {
                        typeface::wrap(node.text(), inner * scale, |ch| {
                            face.glyph(ch, false).advance
                        })
                        .len() as i32
                            * line
                    }
                    None => font.wrapped_height(inner, node.text(), false),
                };
                (width, height)
            }
            widget::LINK => (width - 12, line),
            widget::COMBO => (160, line + 10),
            widget::LIST | widget::TREE | widget::TEXT_EDITOR | widget::CANVAS => (160, 100),
            widget::TABS => (160, 120),
            widget::TOOLBAR => node
                .toolbar
                .as_ref()
                .map_or((0, line), |toolbar| toolbar.natural(font)),
            widget::IMAGE => node
                .image
                .as_ref()
                .map_or((32, 32), |image| (image.width(), image.height())),
            widget::REBAR => {
                let (left, _, right, _) = super::rebar::Rebar::content_inset();
                // 与带区布局一致：隐藏的带区（如关闭的地址栏）不占行。
                let sizes: Vec<_> = children
                    .iter()
                    .filter(|id| self.nodes[*id].visible())
                    .map(|id| self.natural(*id, available_width, font, scale))
                    .collect();
                (
                    sizes.iter().map(|size| size.0).max().unwrap_or(0) + left + right,
                    node.rebar.as_ref().map_or(0, |rebar| {
                        rebar.height(&sizes.iter().map(|size| size.1).collect::<Vec<_>>())
                    }),
                )
            }
            widget::CALENDAR => (224, (line + 7) * 8),
            widget::MENU_BAR => (width, line + 4),
            _ => (width, line + 10),
        };
        let padding = node.inset().saturating_mul(2);
        let top = if node.kind == widget::GROUP {
            line + 8
        } else {
            0
        };
        let width = if node.layout.width >= 0 {
            node.layout.width
        } else {
            natural.0.saturating_add(padding)
        };
        let height = if node.layout.height >= 0 {
            node.layout.height
        } else {
            natural.1.saturating_add(padding).saturating_add(top)
        };
        (
            width.max(node.layout.min_width),
            height.max(node.layout.min_height),
        )
    }
    fn place_children(
        &mut self,
        children: &[u64],
        bounds: Rect,
        clip: Rect,
        row: bool,
        gap: i32,
        font: &Painter<'_>,
    ) {
        let scale = font.scale();
        let visible: Vec<_> = children
            .iter()
            .filter(|id| self.nodes[id].state & state::HIDDEN == 0)
            .copied()
            .collect();
        let mut sizes: Vec<_> = visible
            .iter()
            .map(|id| self.natural(*id, bounds.width, font, scale))
            .collect();
        let available = if row { bounds.width } else { bounds.height };
        let used = sizes
            .iter()
            .map(|s| if row { s.0 } else { s.1 })
            .fold(0i64, |a, b| a + i64::from(b))
            + i64::from(gap) * visible.len().saturating_sub(1) as i64;
        // 先从可伸缩项收回超出的空间，最多收回到声明的 minimum；真正的最小尺寸溢出仍由父区域裁剪。
        // 按可收回容量累计取整，只遍历两次；指针事件复用布局，不在热路径反复分配排版结果。
        let capacity: Vec<i64> = visible
            .iter()
            .zip(&sizes)
            .map(|(id, size)| {
                let rule = self.nodes[id].layout;
                if rule.grow == 0 {
                    0
                } else {
                    i64::from(if row {
                        size.0 - rule.min_width
                    } else {
                        size.1 - rule.min_height
                    })
                    .max(0)
                }
            })
            .collect();
        let total_capacity: i64 = capacity.iter().sum();
        let deficit = (used - i64::from(available)).max(0).min(total_capacity);
        if deficit != 0 {
            let mut accumulated = 0;
            let mut prior = 0;
            for (size, capacity) in sizes.iter_mut().zip(capacity) {
                accumulated += capacity;
                let cumulative = deficit * accumulated / total_capacity;
                let reduction = (cumulative - prior) as i32;
                prior = cumulative;
                if row {
                    size.0 -= reduction;
                } else {
                    size.1 -= reduction;
                }
            }
        }
        let extra = (i64::from(available) - used + deficit).max(0);
        let total: u64 = visible
            .iter()
            .map(|id| u64::from(self.nodes[id].layout.grow))
            .sum();
        let mut weight = 0u64;
        let mut prior = 0i64;
        let mut cursor = if row { bounds.x } else { bounds.y };
        for (index, id) in visible.into_iter().enumerate() {
            weight += u64::from(self.nodes[&id].layout.grow);
            let cumulative = if total == 0 {
                0
            } else {
                extra * weight as i64 / total as i64
            };
            let addition = (cumulative - prior) as i32;
            prior = cumulative;
            let node = &self.nodes[&id];
            let rule = node.layout;
            let rect = if row {
                Rect::new(
                    cursor,
                    bounds.y,
                    sizes[index].0.saturating_add(addition),
                    if rule.height < 0 {
                        bounds.height.max(rule.min_height)
                    } else {
                        sizes[index].1
                    },
                )
            } else {
                Rect::new(
                    bounds.x,
                    cursor,
                    if rule.width < 0 {
                        bounds.width.max(rule.min_width)
                    } else {
                        sizes[index].0
                    },
                    sizes[index].1.saturating_add(addition),
                )
            };
            cursor = cursor
                .saturating_add(if row { rect.width } else { rect.height })
                .saturating_add(gap);
            let intersection = intersect(rect, clip);
            let strip = (self.nodes[&id].kind == widget::TABS).then(|| {
                let pages: Vec<_> = self.nodes[&id]
                    .children
                    .iter()
                    .filter(|page| self.nodes[page].visible())
                    .map(|page| {
                        let text = self.nodes[page].text();
                        (
                            *page,
                            ((font.text_width(text, false) + scale - 1) / scale + 20).max(48),
                        )
                    })
                    .collect();
                super::tab_strip::TabStrip::new(
                    rect.inset(rule.padding),
                    font.line_height(),
                    &pages,
                )
            });
            // 隐藏或删除的选中页失效后回到第一个可用页；禁用页保留表头但不作为默认页。
            let fallback = strip.as_ref().and_then(|strip| {
                strip
                    .headers
                    .iter()
                    .map(|(page, _)| *page)
                    .find(|page| self.nodes[page].enabled())
                    .or_else(|| strip.headers.first().map(|(page, _)| *page))
            });
            let node = self.nodes.get_mut(&id).expect("layout child");
            if let Some(strip) = strip {
                node.selected = node
                    .selected
                    .filter(|selected| strip.headers.iter().any(|(page, _)| page == selected))
                    .or(fallback);
                node.tabs = strip;
            }
            node.rect = rect;
            node.clip = intersection;
            node.row_height = font.line_height() + 6;
            if node.kind == widget::LIST {
                // 小图标/列表的条目宽度取最宽标签 + 图标与内边距（comctl32 按最宽项排列），大图标取主题图标间距。
                let widest = (0..node.row_count())
                    .filter_map(|index| node.row_at(index))
                    .map(|(_, text)| {
                        let label = text.split('\t').next().unwrap_or("");
                        (font.text_width(label, false) + scale - 1) / scale
                    })
                    .max()
                    .unwrap_or(0);
                let metrics = &font.theme().window;
                if let Some(table) = node.table.as_mut() {
                    table.cells = (
                        (metrics.icon_spacing, metrics.icon_vertical_spacing),
                        (widest + 2 + 16 + 2 + 4 + 12).min(rect.width.max(1)),
                    );
                }
            }
            if matches!(node.kind, widget::LIST | widget::TREE) {
                if node
                    .table
                    .as_ref()
                    .is_some_and(|table| table.columns.is_empty())
                {
                    let width = (0..node.row_count())
                        .map(|index| {
                            let (id, text) = node.row_at(index).expect("visible row");
                            let text = (font.text_width(text, false) + scale - 1) / scale;
                            let icon = node
                                .row_icons
                                .get(id)
                                .is_some_and(|icons| icons.0.is_some());
                            match &node.outline {
                                // 树与绘制共用行几何：缩进列、图标与标签留白。
                                Some(outline) => {
                                    outline.row(Rect::new(0, 0, 0, 0), index, icon).label
                                        + super::outline::Outline::label_width(text)
                                        + 2
                                }
                                None => text + 8 + if icon { 20 } else { 0 },
                            }
                        })
                        .max()
                        .unwrap_or(0);
                    node.table.as_mut().expect("list model").content_width = width;
                }
                node.clamp_list();
            }
            if let Some(editor) = &mut node.editor {
                editor.layout(rect, font);
            }
            if let Some(toolbar) = &mut node.toolbar {
                toolbar.layout(rect, font);
            }
            let kind = node.kind;
            let nested = node.children.clone();
            let selected = node.selected;
            let mut inner = rect.inset(node.inset());
            if kind == widget::GROUP {
                let top = font.line_height() + 8;
                inner.y += top;
                inner.height = (inner.height - top).max(0);
            }
            if kind == widget::REBAR {
                let bands: Vec<u64> = nested
                    .iter()
                    .copied()
                    .filter(|band| self.nodes[band].visible())
                    .collect();
                let heights: Vec<i32> = bands
                    .iter()
                    .map(|band| self.natural(*band, rect.width, font, scale).1)
                    .collect();
                let areas = self
                    .nodes
                    .get_mut(&id)
                    .and_then(|node| node.rebar.as_mut())
                    .map(|rebar| rebar.layout(rect, &heights))
                    .unwrap_or_default();
                for (band, area) in bands.into_iter().zip(areas) {
                    self.place_children(
                        &[band],
                        area,
                        intersect(area, intersection),
                        false,
                        0,
                        font,
                    );
                }
                continue;
            }
            if kind == widget::TABS {
                inner = node.tabs.page_area();
                if let Some(page) = selected {
                    self.place_children(
                        &[page],
                        inner,
                        intersect(inner, intersection),
                        false,
                        rule.gap,
                        font,
                    );
                }
            } else {
                self.place_children(
                    &nested,
                    inner,
                    intersect(inner, intersection),
                    kind == widget::ROW,
                    rule.gap,
                    font,
                );
            }
        }
    }
}
pub fn intersect(a: Rect, b: Rect) -> Rect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    Rect::new(
        x,
        y,
        (a.right().min(b.right()) - x).max(0),
        (a.bottom().min(b.bottom()) - y).max(0),
    )
}
