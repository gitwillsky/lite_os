//! 公历月历（Windows 2000 timedate.cpl 的 CalWnd）：只有星期表头与日期格，月份与年份由应用用组合框和微调钮设定；
//! 统一几何、日期选择和键盘交互，应用只处理选定日期的业务含义。
use super::widgets::Tree;
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{EventKind, Ink, Painter};
use classic_ui_abi::{self as abi, Date, event, widget};
pub struct Calendar {
    pub date: Date,
    pressed: Option<u32>,
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
            pressed: None,
        }
    }
    pub fn set(&mut self, date: Date) {
        self.date = date;
        self.pressed = None;
    }
    fn offset(&self) -> usize {
        calendar::weekday(calendar::days_from_civil(calendar::Date {
            year: i64::from(self.date.year),
            month: self.date.month,
            day: 1,
        })) as usize
    }
    /// 第 `index` 格（0–6 为星期表头，其后按星期排的日期）：凹陷边框以内均分 7 列 7 行。
    fn cell(rect: Rect, index: usize) -> Rect {
        let body = rect.inset(2);
        let (column, row) = ((index % 7) as i32, (index / 7) as i32);
        Rect::new(
            body.x + body.width * column / 7,
            body.y + body.height * row / 7,
            body.width * (column + 1) / 7 - body.width * column / 7,
            body.height * (row + 1) / 7 - body.height * row / 7,
        )
    }
    fn at(&self, rect: Rect, point: (i32, i32)) -> Option<u32> {
        (1..=calendar::days_in_month(i64::from(self.date.year), self.date.month)).find(|day| {
            contains(
                Self::cell(rect, 7 + self.offset() + *day as usize - 1),
                point,
            )
        })
    }
    pub fn paint(&self, rect: Rect, p: &mut Painter<'_>, focused: bool) {
        p.edge(rect, Edge::Sunken);
        p.fill(rect.inset(2), C::Window);
        // 星期表头：按钮阴影色底、高光色字。
        let header = Self::cell(rect, 0);
        p.fill(
            Rect::new(header.x, header.y, rect.inset(2).width, header.height),
            C::ButtonShadow,
        );
        for (index, title) in ["日", "一", "二", "三", "四", "五", "六"]
            .iter()
            .enumerate()
        {
            p.text_centered(
                Self::cell(rect, index),
                title,
                Ink::Color(C::ButtonHilight),
                false,
            );
        }
        for day in 1..=calendar::days_in_month(i64::from(self.date.year), self.date.month) {
            let area = Self::cell(rect, 7 + self.offset() + day as usize - 1);
            let selected = self.date.day == day;
            if selected {
                p.fill(area, C::Hilight);
            }
            p.text_centered(
                area,
                &day.to_string(),
                Ink::Color(if selected {
                    C::HilightText
                } else {
                    C::WindowText
                }),
                false,
            );
            if selected && focused {
                p.focus_rect(area);
            }
        }
    }
}
impl Tree {
    pub fn calendar_input(
        &mut self,
        window: u64,
        input: &EventKind,
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
                calendar.pressed = calendar.at(node.rect, self.pointer);
                self.armed = Some(id);
                self.focus = Some(id);
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            } => {
                if let Some(day) = calendar
                    .pressed
                    .take()
                    .filter(|day| Some(*day) == calendar.at(node.rect, self.pointer))
                {
                    calendar.date.day = day;
                }
                self.armed = None;
            }
            EventKind::PointerLeave | EventKind::Focus(false) => {
                calendar.pressed = None;
                self.armed = None;
            }
            EventKind::Key(key) if key.pressed && !key.modifiers.control && !key.modifiers.alt => {
                match key.code {
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
