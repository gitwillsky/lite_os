//! 控件绘制只消费布局结果；应用自绘是运行库执行的批次，不接收裸 Painter。
use super::widgets::{Node, Tree};
use canvas::Rect;
use classic_theme::{Edge, Rgb, SystemColor as C};
use classic_ui::{ButtonState, Ink, Painter};
use classic_ui_abi::{state, widget};
impl Tree {
    pub fn paint(
        &self,
        p: &mut Painter<'_>,
        keyboard_focus: bool,
        mono: Option<&terminal_font::TerminalFont>,
    ) {
        if let Some(menu) = self
            .menubar
            .and_then(|id| self.nodes.get(&id))
            .and_then(|node| node.menu.as_ref())
        {
            p.set_cues(menu.state.cues_visible());
        }
        for id in self.roots() {
            self.paint_node(id, p, keyboard_focus, false, mono);
        }
        if let Some(node) = self
            .menubar
            .and_then(|id| self.nodes.get(&id))
            .filter(|node| node.visible() && !node.clip.is_empty())
            && let Some(menu) = &node.menu
        {
            menu.state.paint(p, node.rect, &menu.entries, self.pointer);
        }
        // 打开的组合框下拉列表覆盖在全部控件之上（菜单之前，菜单打开时列表已关闭）。
        for node in self.nodes.values() {
            if node.combo.as_ref().is_some_and(|combo| combo.open) {
                node.paint_combo_list(p);
            }
        }
        if let Some(node) = self.popup.and_then(|id| self.nodes.get(&id))
            && let Some(menu) = &node.menu
            && menu.state.active()
        {
            menu.state.paint(p, node.rect, &menu.entries, self.pointer);
        }
        // 工具提示画在最上层。
        if let Some((bounds, _)) = self.layout_bounds {
            let now = std::time::Instant::now();
            for node in self.nodes.values().filter(|node| node.visible()) {
                if let Some(toolbar) = &node.toolbar {
                    toolbar.paint_tip(p, self.pointer, bounds, now);
                }
            }
        }
    }
    fn paint_node(
        &self,
        id: u64,
        p: &mut Painter<'_>,
        keyboard_focus: bool,
        inherited_disabled: bool,
        mono: Option<&terminal_font::TerminalFont>,
    ) {
        let node = &self.nodes[&id];
        if !node.visible() || node.clip.is_empty() {
            return;
        }
        p.clipped(node.clip, |p| {
            let rect = node.rect;
            let disabled = inherited_disabled || !node.enabled();
            let ink = if disabled {
                Ink::Disabled
            } else {
                Ink::Color(C::ButtonText)
            };
            if matches!(node.kind, widget::COLUMN | widget::ROW)
                && node.state & state::CLIENT_EDGE != 0
            {
                p.edge(rect, Edge::Sunken);
            }
            if let Some((color, image)) = &node.background {
                // 背景在客户区边框以内。
                let area = rect.inset(node.inset() - node.layout.padding);
                if let Some(color) = color {
                    p.fill_rgb(area, *color);
                }
                if let Some(image) = image {
                    image.paint(p, area.x, area.y);
                }
            }
            match node.kind {
                widget::BUTTON => p.push_button(
                    rect,
                    node.text(),
                    ButtonState {
                        pressed: self.armed == Some(id)
                            && node.contains(self.pointer.0, self.pointer.1),
                        disabled,
                        default: node.state & state::DEFAULT != 0,
                        focused: self.focus == Some(id) && keyboard_focus && !disabled,
                    },
                ),
                widget::LINK => {
                    let anchor = p.theme().anchors.link;
                    let focused = self.focus == Some(id) && keyboard_focus && !disabled;
                    let draw = |p: &mut Painter<'_>| {
                        let shown = p.fit(node.text(), rect.width);
                        let width = (p.text_width(&shown, false) + p.scale() - 1) / p.scale();
                        let y = rect.y + (rect.height - p.line_height()) / 2;
                        let ink = if disabled {
                            Ink::Disabled
                        } else {
                            Ink::Rgb(anchor)
                        };
                        p.text(rect.x, y, &shown, ink, false);
                        // IE 链接下划线：基线下方 1 像素。
                        let underline = y + p.ascent() + 1;
                        if !disabled {
                            p.fill_rgb(Rect::new(rect.x, underline, width, 1), anchor);
                        }
                        if focused {
                            p.focus_rect(Rect::new(
                                rect.x - 1,
                                y - 1,
                                width + 2,
                                p.line_height() + 2,
                            ));
                        }
                    };
                    match node.logical_font(&p.theme().fonts) {
                        Some(logical) => {
                            let face = p.fonts().custom(&logical);
                            p.with_font(&face, draw);
                        }
                        None => draw(p),
                    }
                }
                widget::COMBO => node.paint_combo(
                    p,
                    self.focus == Some(id) && keyboard_focus && !disabled,
                    !disabled,
                ),
                widget::LABEL if node.font.is_some() => {
                    let logical = node.logical_font(&p.theme().fonts).expect("label font");
                    let face = p.fonts().custom(&logical);
                    p.with_font(&face, |p| {
                        if node.state & state::WRAP != 0 {
                            p.text_wrapped(rect, node.text(), ink, false);
                        } else {
                            p.label_fit(
                                rect.x,
                                rect.y + (rect.height - p.line_height()) / 2,
                                rect.width,
                                node.text(),
                                ink,
                            );
                        }
                    });
                }
                widget::LABEL => {
                    if node.state & state::WRAP != 0 {
                        p.text_wrapped(rect, node.text(), ink, false);
                    } else {
                        // 右对齐与左对齐共用截断规则：先按矩形宽度截断，再把实际文字宽度贴到右边。
                        let x = if node.state & state::ALIGN_RIGHT != 0 {
                            let shown = p.fit(node.text(), rect.width);
                            let width = (p.text_width(&shown, false) + p.scale() - 1) / p.scale();
                            rect.x + (rect.width - width).max(0)
                        } else {
                            rect.x
                        };
                        p.label_fit(
                            x,
                            rect.y + (rect.height - p.line_height()) / 2,
                            rect.right() - x,
                            node.text(),
                            ink,
                        );
                    }
                }
                widget::CHECKBOX | widget::RADIO => {
                    let y = rect.y + (rect.height - 13) / 2;
                    let checked = node.state & state::CHECKED != 0;
                    if node.kind == widget::RADIO {
                        p.radio(rect.x, y, checked, disabled);
                    } else {
                        p.checkbox(rect.x, y, checked, disabled);
                    }
                    let text_y = rect.y + (rect.height - p.line_height()) / 2;
                    let available = (rect.width - 18).max(0);
                    p.label_fit(rect.x + 18, text_y, available, node.text(), ink);
                    // Win32 BS_CHECKBOX/BS_RADIOBUTTON 只框文字，外扩 1 像素；不框整个控件。
                    if self.focus == Some(id) && keyboard_focus && !disabled {
                        let shown = p.fit(node.text(), available);
                        let width = (p.text_width(&shown, false) + p.scale() - 1) / p.scale();
                        p.focus_rect(Rect::new(
                            rect.x + 17,
                            text_y - 1,
                            (width + 2).min(available + 1),
                            p.line_height() + 2,
                        ));
                    }
                }
                widget::EDIT if node.state & state::BORDERLESS != 0 => p.edit_borderless(
                    rect,
                    node.edit.as_ref().expect("edit node"),
                    self.focus == Some(id) && keyboard_focus,
                    !disabled,
                ),
                widget::EDIT => {
                    p.edit_enabled(
                        rect,
                        node.edit.as_ref().expect("edit node"),
                        self.focus == Some(id) && keyboard_focus,
                        node.state & state::PASSWORD != 0,
                        !disabled,
                    );
                }
                widget::TEXT_EDITOR => node.editor.as_ref().expect("text editor").paint(
                    rect,
                    p,
                    self.focus == Some(id) && keyboard_focus && !disabled,
                ),
                widget::IMAGE => {
                    if let Some(image) = &node.image {
                        if image.width() < rect.width && image.height() == rect.height {
                            image.paint_stretched(p, rect.x, rect.y, rect.width);
                        } else {
                            image.paint(p, rect.x, rect.y);
                        }
                    }
                }
                widget::REBAR => {
                    if let Some(rebar) = &node.rebar {
                        rebar.paint(p, rect);
                    }
                }
                widget::GROUP => p.group_box(rect, node.text()),
                widget::CALENDAR => node.calendar.as_ref().expect("calendar model").paint(
                    rect,
                    p,
                    self.focus == Some(id) && keyboard_focus && !disabled,
                ),
                widget::STATUS => {
                    let panes: Vec<&str> = node.text().split('\t').collect();
                    let icons: Vec<_> = (1..=panes.len() as u64)
                        .map(|pane| {
                            node.row_icons
                                .get(&pane)
                                .and_then(|icons| icons.0.as_deref())
                        })
                        .collect();
                    p.status_bar(
                        rect,
                        &panes,
                        classic_ui::StatusLayout {
                            parts: &node.status_parts,
                            icons: &icons,
                            grip: self.grip_rect() == Some(node.grip_area()),
                        },
                    );
                }
                widget::LIST | widget::TREE => paint_list(
                    node,
                    p,
                    self.focus == Some(id) && keyboard_focus && !disabled,
                ),
                widget::TABS => {
                    let strip = &node.tabs;
                    p.edge(strip.body, Edge::RaisedSoft);
                    // 选中表头最后画：它覆盖相邻表头各 2 像素并擦除主体上边框。
                    let pages = strip
                        .headers
                        .iter()
                        .map(|(page, _)| *page)
                        .filter(|page| node.selected != Some(*page))
                        .chain(node.selected);
                    for page in pages {
                        let Some(header) = strip.header(page, node.selected) else {
                            continue;
                        };
                        let selected = node.selected == Some(page);
                        p.tab(
                            header,
                            strip.label(header),
                            self.nodes[&page].text(),
                            if disabled || !self.nodes[&page].enabled() {
                                Ink::Disabled
                            } else {
                                Ink::Color(C::ButtonText)
                            },
                            selected && self.focus == Some(id) && keyboard_focus && !disabled,
                        );
                    }
                }
                widget::TOOLBAR => {
                    if let Some(toolbar) = &node.toolbar {
                        toolbar.paint(p, disabled);
                    }
                }
                widget::UPDOWN => node.paint_updown(p, disabled),
                widget::SCROLLBAR => {
                    p.scrollbar(
                        rect,
                        node.range.vertical,
                        (
                            node.range.maximum - node.range.minimum,
                            node.range.page,
                            node.position.position,
                        ),
                        node.position.pressed(),
                    );
                }
                widget::SLIDER => {
                    p.trackbar(
                        rect,
                        node.range.vertical,
                        (
                            node.position.position,
                            node.range.maximum - node.range.minimum,
                        ),
                        10,
                    );
                    if self.focus == Some(id) && keyboard_focus && !disabled {
                        p.focus_rect(rect.inset(1));
                    }
                }
                widget::CANVAS => {
                    for command in &node.drawing {
                        let r = Rect::new(
                            rect.x.saturating_add(command.rect.x),
                            rect.y.saturating_add(command.rect.y),
                            command.rect.width,
                            command.rect.height,
                        );
                        if r.is_empty() {
                            continue;
                        }
                        let rgb = Rgb {
                            red: (command.color >> 16) as u8,
                            green: (command.color >> 8) as u8,
                            blue: command.color as u8,
                        };
                        match command.kind {
                            1 => p.fill_rgb(r, rgb),
                            2 => {
                                p.fill_rgb(Rect::new(r.x, r.y, r.width, 1), rgb);
                                p.fill_rgb(Rect::new(r.x, r.bottom() - 1, r.width, 1), rgb);
                                p.fill_rgb(Rect::new(r.x, r.y, 1, r.height), rgb);
                                p.fill_rgb(Rect::new(r.right() - 1, r.y, 1, r.height), rgb);
                            }
                            3 => p.clipped(r, |p| {
                                p.text(r.x, r.y, &command.text, Ink::Rgb(rgb), command.bold);
                            }),
                            5 => {
                                if let Some(font) = mono {
                                    p.monospace(r, &command.text, font, rgb, command.bold);
                                }
                            }
                            4 => {
                                p.text_wrapped(r, &command.text, Ink::Rgb(rgb), command.bold);
                            }
                            6 => p.text_centered(r, &command.text, Ink::Rgb(rgb), command.bold),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if node
                .label_edit
                .as_ref()
                .is_some_and(|editing| !editing.pending)
                && let (Some(area), Some(edit)) = (node.label_rect(), node.edit.as_ref())
            {
                p.clipped(node.list_geometry().rows, |p| {
                    p.edit(area, edit, self.focus == Some(id) && keyboard_focus, false);
                });
            }
            for child in &node.children {
                self.paint_node(*child, p, keyboard_focus, disabled, mono);
            }
        });
    }
}
fn paint_list(node: &Node, p: &mut Painter<'_>, focused: bool) {
    let inner = if node.list_edge() == 0 {
        node.rect
    } else {
        p.edge(node.rect, Edge::Sunken)
    };
    p.fill(inner, C::Window);
    let geometry = node.list_geometry();
    let table = node.table.as_ref().expect("list model");
    if let Some(grid) = geometry.grid {
        paint_grid(node, p, focused, grid);
        return;
    }
    if !geometry.header.is_empty() {
        p.clipped(geometry.header,|p|table.header.paint(p,geometry.shifted_header,|p,index,rect|p.list_header_cell(rect,&table.columns[index].1,matches!(table.gesture,Some(super::table::Gesture::Column(column)) if column==index)))) ;
        // 最后一列右侧到表头右缘画一个空白表头格（comctl32 表头的填充区），不露出控件背景。
        let end = geometry.shifted_header.x + table.header.total();
        if end < geometry.header.right() {
            let filler = Rect::new(
                end,
                geometry.header.y,
                geometry.header.right() - end,
                geometry.header.height,
            );
            p.clipped(geometry.header, |p| p.list_header_cell(filler, "", false));
        }
    }
    p.clipped(geometry.rows, |p| {
        let count =
            ((geometry.rows.height + node.row_height - 1) / node.row_height).max(0) as usize;
        for (index, absolute) in (node.position.position as usize..node.row_count())
            .take(count)
            .enumerate()
        {
            let (id, text) = node.row_at(absolute).expect("visible row");
            let editing = node.editing_label(*id);
            let row = Rect::new(
                geometry.rows.x,
                geometry.rows.y + index as i32 * node.row_height,
                geometry.rows.width,
                node.row_height,
            );
            let selected = node.row_selected(*id);
            let image = node.row_icons.get(id).and_then(|icons| icons.0.as_ref());
            if let Some(outline) = node.outline.as_ref().filter(|_| table.columns.is_empty()) {
                paint_tree_row(
                    p,
                    outline,
                    Rect::new(
                        row.x - table.horizontal.position,
                        row.y,
                        row.width,
                        row.height,
                    ),
                    absolute,
                    (!editing).then_some(text.as_str()),
                    image,
                    selected,
                    focused,
                    focused && node.selected == Some(*id),
                );
                continue;
            }
            // comctl32 默认只高亮首列标签（图标与高亮色混合）；LVS_EX_FULLROWSELECT 时整行高亮。
            let caret = focused && node.selected == Some(*id);
            // 没有列的单列列表按列表框（LISTBOX）整行高亮。
            let full_row = table.full_row || table.columns.is_empty();
            if selected && full_row {
                p.fill(row, if focused { C::Hilight } else { C::ButtonFace });
            }
            let look = classic_ui::ItemLook {
                selected: selected && !full_row,
                active: focused,
                focused: caret && !full_row,
                text: C::WindowText,
            };
            let ink = Ink::Color(if selected && focused && full_row {
                C::HilightText
            } else {
                C::WindowText
            });
            if table.columns.is_empty() {
                let x = row.x - table.horizontal.position;
                if let Some(image) = image {
                    image.paint(p, x + 3, row.y + (row.height - 16) / 2);
                }
                let leading = if image.is_some() { 20 } else { 0 };
                if !editing {
                    p.text_fit(
                        x + 4 + leading,
                        row.y + 3,
                        (row.width - 8 - leading).max(0),
                        text,
                        ink,
                        false,
                    );
                }
            } else {
                let mut x = row.x - table.horizontal.position;
                for (column, text) in text.split('\t').take(table.columns.len()).enumerate() {
                    let width = table.header.width(column);
                    let cell = Rect::new(x, row.y, width, row.height);
                    if column == 0 && !full_row {
                        p.clipped(cell, |p| {
                            p.small_icon_item(
                                Rect::new(cell.x + 1, cell.y, cell.width - 1, cell.height),
                                image.map(|image| &**image),
                                (!editing).then_some(text),
                                look,
                            );
                        });
                        x += width;
                        continue;
                    }
                    p.clipped(cell, |p| {
                        let leading = if column == 0 && image.is_some() {
                            20
                        } else {
                            0
                        };
                        if column == 0
                            && let Some(image) = image
                        {
                            image.paint(p, cell.x + 3, cell.y + (cell.height - 16) / 2);
                        }
                        if column == 0 && editing {
                            return;
                        }
                        let available = (width - 8 - leading).max(0);
                        let shown = p.fit(text, available);
                        let text_width = (p.text_width(&shown, false) + p.scale() - 1) / p.scale();
                        let alignment = table
                            .alignments
                            .get(&table.columns[column].0)
                            .copied()
                            .unwrap_or(0);
                        let offset = match alignment {
                            1 => (available - text_width) / 2,
                            2 => available - text_width,
                            _ => 0,
                        };
                        p.text(
                            x + 4 + leading + offset.max(0),
                            row.y + 3,
                            &shown,
                            ink,
                            false,
                        );
                    });
                    x += width;
                }
            }
            if caret && full_row {
                p.focus_rect(row);
            }
        }
    });
    if !geometry.vertical.is_empty() {
        p.scrollbar(
            geometry.vertical,
            true,
            (
                node.row_count() as i32 - 1,
                (geometry.rows.height / node.row_height).max(1),
                node.position.position,
            ),
            node.position.pressed(),
        );
    }
    if !geometry.horizontal.is_empty() {
        p.scrollbar(
            geometry.horizontal,
            false,
            (
                table.extent() - 1,
                geometry.rows.width,
                table.horizontal.position,
            ),
            table.horizontal.pressed(),
        );
    }
}

/// 一行树项：分支线与展开框、图标、只覆盖文字标签的选择高亮与焦点框（非整行选择）。
/// 失焦时选择保持为按钮面色（TVS_SHOWSELALWAYS），与命中使用同一 RowGeometry。
#[allow(clippy::too_many_arguments)]
fn paint_tree_row(
    p: &mut Painter<'_>,
    outline: &super::outline::Outline,
    row: Rect,
    index: usize,
    text: Option<&str>,
    image: Option<&std::rc::Rc<classic_ui::Bitmap>>,
    selected: bool,
    focused: bool,
    caret: bool,
) {
    use super::outline::Outline;
    outline.paint_lines(p, row, index, image.is_some());
    let geometry = outline.row(row, index, image.is_some());
    if let Some(image) = image {
        image.paint(p, geometry.icon, row.y + (row.height - 16) / 2);
    }
    let Some(text) = text else { return };
    let width = (p.text_width(text, false) + p.scale() - 1) / p.scale();
    let label = Rect::new(
        geometry.label,
        row.y,
        Outline::label_width(width),
        row.height,
    );
    if selected {
        p.fill(label, if focused { C::Hilight } else { C::ButtonFace });
    }
    let ink = Ink::Color(if selected && focused {
        C::HilightText
    } else {
        C::WindowText
    });
    p.text(
        label.x + Outline::text_offset(),
        row.y + (row.height - p.line_height()) / 2,
        text,
        ink,
        false,
    );
    if caret {
        p.focus_rect(label);
    }
}

/// 图标/列表视图的一个条目：大图标按 LVS_ICON，小图标与列表按图标加标签（共享的 comctl32 列表项绘制）。
fn paint_grid_item(
    node: &Node,
    p: &mut Painter<'_>,
    focused: bool,
    grid: super::item_grid::ItemGrid,
    index: usize,
    id: u64,
    text: &str,
) {
    let cell = grid.cell(index, node.position.position);
    let look = classic_ui::ItemLook {
        selected: node.row_selected(id),
        active: focused,
        focused: focused && node.selected == Some(id),
        text: C::WindowText,
    };
    let icons = node.row_icons.get(&id);
    let text = (!node.editing_label(id)).then(|| text.split('\t').next().unwrap_or(""));
    if grid.mode == 1 {
        let icon = icons.and_then(|icons| icons.1.as_deref());
        p.large_icon_item(cell, icon, text, look);
    } else {
        let icon = icons.and_then(|icons| icons.0.as_deref());
        p.small_icon_item(cell, icon, text, look);
    }
}

fn paint_grid(node: &Node, p: &mut Painter<'_>, focused: bool, grid: super::item_grid::ItemGrid) {
    p.clipped(grid.viewport, |p| {
        let caret = node.selected.filter(|_| focused);
        let mut deferred = None;
        for index in grid.visible(node.position.position) {
            let Some((id, text)) = node.row_at(index) else {
                continue;
            };
            // 焦点项的大图标标签显示全文并可盖住下方条目，因此最后画。
            if grid.mode == 1 && caret == Some(*id) {
                deferred = Some(index);
                continue;
            }
            paint_grid_item(node, p, focused, grid, index, *id, text);
        }
        if let Some(index) = deferred
            && let Some((id, text)) = node.row_at(index)
        {
            paint_grid_item(node, p, focused, grid, index, *id, text);
        }
    });
    let geometry = node.list_geometry();
    let bar = if grid.vertical() {
        geometry.vertical
    } else {
        geometry.horizontal
    };
    if !bar.is_empty() {
        p.scrollbar(
            bar,
            grid.vertical(),
            (grid.extent - 1, grid.page(), node.position.position),
            node.position.pressed(),
        );
    }
}

#[cfg(test)]
mod tests;
