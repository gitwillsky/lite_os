//! 背景颜色使用共享列表和图形预览，只发布本页负责的 background 字段。
use crate::{Apply, Page, child, label};
use desktop_settings::Settings;
use liteos_ui::{Draw, Event, Layout, PropertySheet, Rect, Widget, WidgetKind as Kind};
const COLORS: [(&str, Option<[u8; 3]>); 12] = [
    ("(默认)", None),
    ("黑色", Some([0, 0, 0])),
    ("深蓝", Some([0, 0, 128])),
    ("蓝灰", Some([58, 110, 165])),
    ("深青", Some([0, 128, 128])),
    ("绿色", Some([0, 128, 0])),
    ("酒红", Some([128, 0, 0])),
    ("紫色", Some([128, 0, 128])),
    ("橄榄", Some([128, 128, 0])),
    ("深灰", Some([64, 64, 64])),
    ("灰色", Some([128, 128, 128])),
    ("银色", Some([192, 192, 192])),
];

pub struct Display {
    _controls: Vec<Widget>,
    list: Widget,
    preview: Widget,
    saved: Option<[u8; 3]>,
}
impl Display {
    pub fn build(sheet: &PropertySheet, settings: Settings) -> liteos_ui::Result<Self> {
        let page = sheet.page("背景")?;
        let preview = child(
            &page,
            Kind::Canvas,
            "",
            Layout {
                height: 120,
                ..Default::default()
            },
        )?;
        let heading = label(&page, "选择桌面背景颜色：")?;
        let list = child(
            &page,
            Kind::List,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        list.set_rows(
            &COLORS
                .iter()
                .enumerate()
                .map(|(index, (name, _))| (index as u64 + 1, *name))
                .collect::<Vec<_>>(),
        )?;
        list.select(Some(
            COLORS
                .iter()
                .position(|(_, color)| *color == settings.background)
                .unwrap_or(0) as u64
                + 1,
        ))?;
        let view = Self {
            _controls: vec![page, heading],
            list,
            preview,
            saved: settings.background,
        };
        view.preview()?;
        Ok(view)
    }
    fn value(&self) -> liteos_ui::Result<Option<[u8; 3]>> {
        Ok(COLORS
            .get(self.list.selection()?.unwrap_or(1).saturating_sub(1) as usize)
            .and_then(|(_, color)| *color))
    }
    fn preview(&self) -> liteos_ui::Result<()> {
        let area = self.preview.geometry()?;
        if area.width <= 0 || area.height <= 0 {
            return Ok(());
        }
        let color = self.value()?.map_or(
            self.preview.color(liteos_ui::ColorRole::Desktop)?,
            |[r, g, b]| u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b),
        );
        let x = (area.width - 140) / 2;
        self.preview.set_drawing(&[
            Draw {
                kind: 1,
                rect: Rect::new(x, 4, 140, 95),
                color: 0xd4d0c8,
                bold: false,
                text: "",
            },
            Draw {
                kind: 2,
                rect: Rect::new(x, 4, 140, 95),
                color: 0,
                bold: false,
                text: "",
            },
            Draw {
                kind: 1,
                rect: Rect::new(x + 8, 12, 124, 72),
                color,
                bold: false,
                text: "",
            },
            Draw {
                kind: 1,
                rect: Rect::new(x + 48, 99, 44, 8),
                color: 0x808080,
                bold: false,
                text: "",
            },
            Draw {
                kind: 1,
                rect: Rect::new(x + 35, 107, 70, 6),
                color: 0xd4d0c8,
                bold: false,
                text: "",
            },
        ])
    }
}
impl Page for Display {
    fn event(&mut self, _event: &Event) -> liteos_ui::Result<Option<Vec<String>>> {
        self.preview()?;
        Ok(None)
    }
    fn dirty(&self) -> liteos_ui::Result<bool> {
        Ok(self.value()? != self.saved)
    }
    fn apply(&self) -> liteos_ui::Result<Apply> {
        let background = self.value()?;
        Ok(Box::new(move || {
            Settings::update(|settings| settings.background = background)
                .map(drop)
                .map_err(|error| error.to_string())
        }))
    }
    fn committed(&mut self) -> liteos_ui::Result<()> {
        self.saved = self.value()?;
        Ok(())
    }
}
