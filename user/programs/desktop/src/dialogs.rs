//! 外壳对话框；窗口的装饰、configure 与帧仍由 classic-ui 拥有。
use canvas::Rect;
use classic_ui::{
    Configuration, Decoration, DecorationAction, Display, Edit, Event, EventKind, FrameKind,
    WindowId,
};
use std::io;
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Run,
}
pub struct Dialog {
    pub window: WindowId,
    pub kind: Kind,
    pub decoration: Decoration,
    pub edit: Edit,
    pointer: (f64, f64),
    armed: Option<usize>,
}
pub enum Action {
    None,
    Close,
    Run(String),
}
impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Run => "运行",
        }
    }
}
impl Dialog {
    pub fn new(display: &mut Display, kind: Kind) -> io::Result<Self> {
        let size = (440, 210);
        let mut decoration = Decoration::new(FrameKind::Fixed);
        decoration.load_icon("run");
        Ok(Self {
            window: display.create_window(kind.title(), "org.liteos.desktop", size)?,
            kind,
            decoration,
            edit: Edit::default(),
            pointer: (0., 0.),
            armed: None,
        })
    }
    pub fn buttons(&self, c: Configuration) -> Vec<Rect> {
        let y = c.height - 35;
        if self.kind == Kind::Run {
            vec![
                Rect::new(c.width - 175, y, 75, 23),
                Rect::new(c.width - 94, y, 75, 23),
            ]
        } else {
            vec![Rect::new(c.width - 115, y, 100, 23)]
        }
    }
    pub fn event(&mut self, display: &mut Display, event: Event) -> io::Result<Action> {
        let Some(c) = display.configuration(self.window) else {
            return Ok(Action::None);
        };
        match self.decoration.event(&event.kind, c) {
            Some(DecorationAction::Request(r)) => display.request(self.window, r)?,
            Some(DecorationAction::Close) => return Ok(Action::Close),
            None => {}
        }
        match event.kind {
            EventKind::Close => return Ok(Action::Close),
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                self.pointer = (x, y)
            }
            EventKind::Button {
                code: 0x110,
                pressed,
                ..
            } => {
                let hit = self.buttons(c).iter().position(|r| {
                    self.pointer.0 >= f64::from(r.x)
                        && self.pointer.0 < f64::from(r.right())
                        && self.pointer.1 >= f64::from(r.y)
                        && self.pointer.1 < f64::from(r.bottom())
                });
                if pressed {
                    self.armed = hit;
                } else if let Some(index) = self.armed.take().filter(|i| Some(*i) == hit) {
                    return self.activate(index);
                }
            }
            EventKind::Key(key) if key.pressed => match key.code {
                1 => return Ok(Action::Close),
                28 => return self.activate(0),
                _ if self.kind == Kind::Run && self.edit.text().len() < 4096 => {
                    self.edit.key(key);
                }
                _ => {}
            },
            _ => {}
        }
        Ok(Action::None)
    }
    fn activate(&self, index: usize) -> io::Result<Action> {
        match self.kind {
            Kind::Run if index == 0 && !self.edit.text().trim().is_empty() => {
                Ok(Action::Run(self.edit.text().into()))
            }
            _ => Ok(Action::Close),
        }
    }
}
