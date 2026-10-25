//! 任务栏设置草稿由标准复选框持有；应用时只合并本页拥有的字段。
use crate::{Apply, Page, child};
use desktop_settings::Settings;
use liteos_ui::{Draw, Event, Layout, PropertySheet, Rect, Widget, WidgetKind as Kind, state};
pub struct Taskbar {
    _page: Widget,
    preview: Widget,
    options: [Widget; 3],
    saved: [bool; 3],
}
impl Taskbar {
    pub fn build(sheet: &PropertySheet, settings: Settings) -> liteos_ui::Result<Self> {
        let page = sheet.page("任务栏选项")?;
        let preview = child(
            &page,
            Kind::Canvas,
            "",
            Layout {
                height: 90,
                ..Default::default()
            },
        )?;
        let labels = ["总在最前(T)", "自动隐藏(U)", "显示时钟(C)"];
        let saved = [
            settings.always_on_top,
            settings.auto_hide,
            settings.show_clock,
        ];
        let mut options = Vec::new();
        for (label, checked) in labels.into_iter().zip(saved) {
            let option = child(&page, Kind::CheckBox, label, Layout::default())?;
            option.set_state(if checked { state::CHECKED } else { 0 })?;
            options.push(option);
        }
        let view = Self {
            _page: page,
            preview,
            options: options.try_into().ok().expect("three taskbar settings"),
            saved,
        };
        view.preview()?;
        Ok(view)
    }
    fn values(&self) -> liteos_ui::Result<[bool; 3]> {
        Ok([
            self.options[0].state()? & state::CHECKED != 0,
            self.options[1].state()? & state::CHECKED != 0,
            self.options[2].state()? & state::CHECKED != 0,
        ])
    }
    fn preview(&self) -> liteos_ui::Result<()> {
        let area = self.preview.geometry()?;
        if area.width <= 0 || area.height <= 0 {
            return Ok(());
        }
        let [top, hide, clock] = self.values()?;
        let bar = if hide { 2 } else { 16 };
        let mut commands = vec![
            Draw {
                kind: 1,
                rect: Rect::new(0, 0, area.width, area.height),
                color: self.preview.color(liteos_ui::ColorRole::Desktop)?,
                bold: false,
                text: "",
            },
            Draw {
                kind: 1,
                rect: Rect::new(0, area.height - bar, area.width, bar),
                color: 0xd4d0c8,
                bold: false,
                text: "",
            },
        ];
        if !hide {
            commands.push(Draw {
                kind: 3,
                rect: Rect::new(2, area.height - 18, 60, 18),
                color: 0,
                bold: false,
                text: "开始",
            });
        }
        if clock && !hide {
            commands.push(Draw {
                kind: 3,
                rect: Rect::new(area.width - 55, area.height - 18, 55, 18),
                color: 0,
                bold: false,
                text: "12:00",
            });
        }
        if !top {
            commands.push(Draw {
                kind: 1,
                rect: Rect::new(30, 20, (area.width - 60).max(0), area.height - 22),
                color: 0xd4d0c8,
                bold: false,
                text: "",
            });
        }
        self.preview.set_drawing(&commands)
    }
}
impl Page for Taskbar {
    fn event(&mut self, _event: &Event) -> liteos_ui::Result<Option<Vec<String>>> {
        self.preview()?;
        Ok(None)
    }
    fn dirty(&self) -> liteos_ui::Result<bool> {
        Ok(self.values()? != self.saved)
    }
    fn apply(&self) -> liteos_ui::Result<Apply> {
        let [top, hide, clock] = self.values()?;
        Ok(Box::new(move || {
            Settings::update(|settings| {
                settings.always_on_top = top;
                settings.auto_hide = hide;
                settings.show_clock = clock;
            })
            .map(drop)
            .map_err(|error| error.to_string())
        }))
    }
    fn committed(&mut self) -> liteos_ui::Result<()> {
        self.saved = self.values()?;
        Ok(())
    }
}
