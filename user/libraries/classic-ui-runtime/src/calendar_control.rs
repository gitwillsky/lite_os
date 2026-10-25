//! 公历月历的统一几何、翻月、日期选择和键盘交互；应用只处理选定日期的业务含义。
use super::widgets::Tree;
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{ButtonState, EventKind, Ink, Painter};
use classic_ui_abi::{self as abi, Date, event, widget};
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Previous,
    Next,
    Day(u32),
}
pub struct Calendar {
    pub date: Date,
    month: (i32, u32),
    pressed: Option<Part>,
}
pub(super) fn valid(date: Date) -> bool {
    (1..=9999).contains(&date.year)
        && (1..=12).contains(&date.month)
        && (1..=calendar::days_in_month(i64::from(date.year), date.month)).contains(&date.day)
}
fn contains(r: Rect, point: (i32, i32)) -> bool {
    point.0 >= r.x && point.0 < r.right() && point.1 >= r.y && point.1 < r.bottom()
}
impl Calendar {
    pub fn new() -> Self {
        Self {
            date: Date::default(),
            month: (1970, 1),
            pressed: None,
        }
    }
    pub fn set(&mut self, date: Date) {
        self.date = date;
        self.month = (date.year, date.month);
        self.pressed = None;
    }
    fn offset(&self) -> usize {
        calendar::weekday(calendar::days_from_civil(calendar::Date {
            year: i64::from(self.month.0),
            month: self.month.1,
            day: 1,
        })) as usize
    }
    fn geometry(rect: Rect, line: i32) -> (Rect, Rect, Rect, Rect) {
        let inner = rect.inset(3);
        let height = (line + 8).min(inner.height.max(0));
        let header = Rect::new(inner.x, inner.y, inner.width, height);
        (
            header,
            Rect::new(header.x, header.y, 26.min(header.width), height),
            Rect::new(
                (header.right() - 26).max(header.x),
                header.y,
                26.min(header.width),
                height,
            ),
            Rect::new(
                inner.x,
                header.bottom(),
                inner.width,
                (inner.height - height).max(0),
            ),
        )
    }
    fn cell(body: Rect, index: usize) -> Rect {
        let (column, row) = ((index % 7) as i32, (index / 7) as i32);
        Rect::new(
            body.x + body.width * column / 7,
            body.y + body.height * row / 7,
            body.width * (column + 1) / 7 - body.width * column / 7,
            body.height * (row + 1) / 7 - body.height * row / 7,
        )
    }
    fn at(&self, rect: Rect, p: &Painter<'_>, point: (i32, i32)) -> Option<Part> {
        let (_, previous, next, body) = Self::geometry(rect, p.line_height());
        if contains(previous, point) {
            return Some(Part::Previous);
        }
        if contains(next, point) {
            return Some(Part::Next);
        }
        (1..=calendar::days_in_month(i64::from(self.month.0), self.month.1))
            .find(|day| {
                contains(
                    Self::cell(body, 7 + self.offset() + *day as usize - 1),
                    point,
                )
            })
            .map(Part::Day)
    }
    fn step_month(&mut self, step: i32) {
        let current = self.month.0 * 12 + self.month.1 as i32 - 1;
        let next = (current + step).clamp(12, 9999 * 12 + 11);
        self.month = (next / 12, (next % 12 + 1) as u32);
    }
    pub fn paint(&self, rect: Rect, p: &mut Painter<'_>, focused: bool) {
        p.edge(rect, Edge::Sunken);
        p.fill(rect.inset(2), C::Window);
        let (header, previous, next, body) = Self::geometry(rect, p.line_height());
        p.text_centered(
            header,
            &format!("{}年{}月", self.month.0, self.month.1),
            Ink::Color(C::WindowText),
            false,
        );
        for (part, rect, title) in [(Part::Previous, previous, "‹"), (Part::Next, next, "›")] {
            p.push_button(
                rect,
                title,
                ButtonState {
                    pressed: self.pressed == Some(part),
                    ..Default::default()
                },
            );
        }
        for (index, title) in ["日", "一", "二", "三", "四", "五", "六"]
            .iter()
            .enumerate()
        {
            p.text_centered(
                Self::cell(body, index),
                title,
                Ink::Color(C::WindowText),
                false,
            );
        }
        for day in 1..=calendar::days_in_month(i64::from(self.month.0), self.month.1) {
            let area = Self::cell(body, 7 + self.offset() + day as usize - 1);
            let selected = self.date
                == Date {
                    year: self.month.0,
                    month: self.month.1,
                    day,
                };
            if selected {
                p.fill(area, if focused { C::Hilight } else { C::ButtonFace });
            }
            p.text_centered(
                area,
                &day.to_string(),
                Ink::Color(if selected && focused {
                    C::HilightText
                } else {
                    C::WindowText
                }),
                false,
            );
            if selected && focused {
                p.focus_rect(area.inset(1));
            }
        }
    }
}
impl Tree {
    pub fn calendar_input(
        &mut self,
        window: u64,
        input: &EventKind,
        p: &Painter<'_>,
        out: &mut Vec<abi::Event>,
    ) -> bool {
        let id = match input {
            EventKind::Key(_) => self.focus,
            _ => self.armed.or_else(|| self.hit()),
        };
        let Some(id) = id.filter(|id| self.active(*id) && self.nodes[id].kind == widget::CALENDAR)
        else {
            return false;
        };
        let node = self.nodes.get_mut(&id).expect("calendar node");
        let calendar = node.calendar.as_mut().expect("calendar model");
        let before = calendar.date;
        match input {
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } => {
                calendar.pressed = calendar.at(node.rect, p, self.pointer);
                self.armed = Some(id);
                self.focus = Some(id);
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            } => {
                if let Some(part) = calendar
                    .pressed
                    .take()
                    .filter(|part| Some(*part) == calendar.at(node.rect, p, self.pointer))
                {
                    match part {
                        Part::Previous => calendar.step_month(-1),
                        Part::Next => calendar.step_month(1),
                        Part::Day(day) => {
                            calendar.date = Date {
                                year: calendar.month.0,
                                month: calendar.month.1,
                                day,
                            }
                        }
                    }
                }
                self.armed = None;
            }
            EventKind::PointerLeave | EventKind::Focus(false) => {
                calendar.pressed = None;
                self.armed = None;
            }
            EventKind::Key(key) if key.pressed && !key.modifiers.control && !key.modifiers.alt => {
                match key.code {
                    104 => calendar.step_month(-1),
                    109 => calendar.step_month(1),
                    102 => calendar.set(Date {
                        day: 1,
                        ..calendar.date
                    }),
                    107 => calendar.set(Date {
                        day: calendar::days_in_month(
                            i64::from(calendar.date.year),
                            calendar.date.month,
                        ),
                        ..calendar.date
                    }),
                    105 | 106 | 103 | 108 => {
                        let delta = match key.code {
                            105 => -1,
                            106 => 1,
                            103 => -7,
                            _ => 7,
                        };
                        let days = calendar::days_from_civil(calendar::Date {
                            year: i64::from(calendar.date.year),
                            month: calendar.date.month,
                            day: calendar.date.day,
                        });
                        let date = calendar::civil_from_days(days + delta);
                        let date = Date {
                            year: date.year as i32,
                            month: date.month,
                            day: date.day,
                        };
                        if valid(date) {
                            calendar.set(date);
                        }
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
        if calendar.date != before {
            out.push(abi::Event {
                window,
                node: id,
                kind: event::CHANGED,
                ..Default::default()
            });
        }
        true
    }
}
