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
        {
            if let Some(menu) = &node.menu {
                menu.state.paint(p, node.rect, &menu.entries, self.pointer);
            }
        }
        if let Some(node) = self.popup.and_then(|id| self.nodes.get(&id)) {
            if let Some(menu) = &node.menu {
                if menu.state.active() {
                    menu.state.paint(p, node.rect, &menu.entries, self.pointer);
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
                        ..Default::default()
                    },
                ),
                widget::LABEL => {
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
                }
                widget::CHECKBOX | widget::RADIO => {
                    let y = rect.y + (rect.height - 13) / 2;
                    let checked = node.state & state::CHECKED != 0;
                    if node.kind == widget::RADIO {
                        p.radio(rect.x, y, checked, disabled);
                    } else {
                        p.checkbox(rect.x, y, checked, disabled);
                    }
                    p.label_fit(
                        rect.x + 18,
                        rect.y + (rect.height - p.line_height()) / 2,
                        (rect.width - 18).max(0),
                        node.text(),
                        ink,
                    );
                    if self.focus == Some(id) && keyboard_focus && !disabled {
                        p.focus_rect(rect.inset(1));
                    }
                }
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
                        image.paint(p, rect.x, rect.y);
                    }
                }
                widget::GROUP => p.group_box(rect, node.text()),
                widget::CALENDAR => node.calendar.as_ref().expect("calendar model").paint(
                    rect,
                    p,
                    self.focus == Some(id) && keyboard_focus && !disabled,
                ),
                widget::STATUS => p.status_bar(rect, &node.text().split('\t').collect::<Vec<_>>()),
                widget::LIST | widget::TREE => paint_list(
                    node,
                    p,
                    self.focus == Some(id) && keyboard_focus && !disabled,
                ),
                widget::TABS => {
                    let body = Rect::new(
                        rect.x,
                        rect.y + p.line_height() + 6,
                        rect.width,
                        (rect.height - p.line_height() - 6).max(0),
                    );
                    p.edge(body, Edge::Raised);
                    for (child, area) in &node.tab_headers {
                        p.tab(
                            *area,
                            self.nodes[child].text(),
                            node.selected == Some(*child),
                        );
                    }
                }
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
            {
                if let (Some(area), Some(edit)) = (node.label_rect(), node.edit.as_ref()) {
                    p.clipped(node.list_geometry().rows, |p| {
                        p.edit(area, edit, self.focus == Some(id) && keyboard_focus, false);
                    });
                }
            }
            for child in &node.children {
                self.paint_node(*child, p, keyboard_focus, disabled, mono);
            }
        });
    }
}
fn paint_list(node: &Node, p: &mut Painter<'_>, focused: bool) {
    let inner = p.edge(node.rect, Edge::Sunken);
    p.fill(inner, C::Window);
    let geometry = node.list_geometry();
    let table = node.table.as_ref().expect("list model");
    if let Some(grid) = geometry.grid {
        paint_grid(node, p, focused, grid);
        return;
    }
    if !geometry.header.is_empty() {
        p.clipped(geometry.header,|p|table.header.paint(p,geometry.shifted_header,|p,index,rect|p.list_header_cell(rect,&table.columns[index].1,table.sort.filter(|(id,_)|*id==table.columns[index].0).map(|(_,ascending)|ascending),matches!(table.gesture,Some(super::table::Gesture::Column(column)) if column==index)))) ;
    }
    p.clipped(geometry.rows, |p| {
        let count =
            ((geometry.rows.height + node.row_height - 1) / node.row_height).max(0) as usize;
        for (index, absolute) in (node.position.position as usize..node.row_count())
            .take(count)
            .enumerate()
        {
            let (id, text) = node.row_at(absolute).expect("visible row");
            let row = Rect::new(
                geometry.rows.x,
                geometry.rows.y + index as i32 * node.row_height,
                geometry.rows.width,
                node.row_height,
            );
            let selected = node.row_selected(*id);
            if selected {
                p.fill(row, if focused { C::Hilight } else { C::ButtonFace });
            }
            let ink = Ink::Color(if selected && focused {
                C::HilightText
            } else {
                C::WindowText
            });
            let image = node.row_icons.get(id).and_then(|icons| icons.0.as_ref());
            let icon_width = if image.is_some() { 20 } else { 0 };
            if let Some(image) = image {
                let clip = if table.columns.is_empty() {
                    row
                } else {
                    Rect::new(
                        row.x - table.horizontal.position,
                        row.y,
                        table.header.width(0),
                        row.height,
                    )
                };
                p.clipped(clip, |p| {
                    image.paint(
                        p,
                        row.x + 3 - table.horizontal.position
                            + node
                                .outline
                                .as_ref()
                                .map_or(0, |outline| outline.indent(absolute)),
                        row.y + (row.height - 16) / 2,
                    )
                });
            }
            if table.columns.is_empty() {
                let indent = node
                    .outline
                    .as_ref()
                    .map_or(0, |outline| outline.indent(absolute));
                if let Some(outline) = &node.outline {
                    outline.paint_expander(
                        p,
                        Rect::new(
                            row.x - table.horizontal.position,
                            row.y,
                            row.width,
                            row.height,
                        ),
                        absolute,
                    );
                }
                p.text_fit(
                    row.x + 4 + indent + icon_width - table.horizontal.position,
                    row.y + 3,
                    (row.width - 8 - indent - icon_width).max(0),
                    text,
                    ink,
                    false,
                );
            } else {
                let mut x = row.x - table.horizontal.position;
                for (column, text) in text.split('\t').take(table.columns.len()).enumerate() {
                    let width = table.header.width(column);
                    p.clipped(Rect::new(x, row.y, width, row.height), |p| {
                        let leading = if column == 0 { icon_width } else { 0 };
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

fn paint_grid(node: &Node, p: &mut Painter<'_>, focused: bool, grid: super::item_grid::ItemGrid) {
    p.clipped(grid.viewport, |p| {
        for index in grid.visible(node.position.position) {
            let Some((id, text)) = node.row_at(index) else {
                continue;
            };
            let cell = grid.cell(index, node.position.position);
            let selected = node.row_selected(*id);
            let ink = Ink::Color(if selected && focused {
                C::HilightText
            } else {
                C::WindowText
            });
            if selected {
                p.fill(cell, if focused { C::Hilight } else { C::ButtonFace });
            }
            let image = node.row_icons.get(id).and_then(|icons| {
                if grid.mode == 1 {
                    icons.1.as_ref()
                } else {
                    icons.0.as_ref()
                }
            });
            let text = text.split('\t').next().unwrap_or("");
            if grid.mode == 1 {
                if let Some(image) = image {
                    image.paint(p, cell.x + (cell.width - 32) / 2, cell.y + 2);
                }
                p.text_wrapped(
                    Rect::new(cell.x + 3, cell.y + 36, cell.width - 6, cell.height - 36),
                    text,
                    ink,
                    false,
                );
            } else {
                if let Some(image) = image {
                    image.paint(p, cell.x + 2, cell.y + (cell.height - 16) / 2);
                }
                let leading = if image.is_some() { 22 } else { 4 };
                p.text_fit(
                    cell.x + leading,
                    cell.y + 3,
                    (cell.width - leading - 4).max(0),
                    text,
                    ink,
                    false,
                );
            }
            if focused && node.selected == Some(*id) {
                p.focus_rect(cell.inset(1));
            }
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
