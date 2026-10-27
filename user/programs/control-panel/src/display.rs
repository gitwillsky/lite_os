//! “显示属性 > 背景”：按 Windows 2000 shell32 背景页选择墙纸与显示方式，只发布本页负责的
//! background 字段。监视器预览由运行库按与桌面相同的墙纸合成规则绘制。
use crate::{Activity, Apply, Page, Response, child, label};
use desktop_settings::{Settings, WallpaperStyle};
use liteos_ui::{
    Dialogs, Draw, Event, EventKind, FileDialogMode, FileFilter, Layout, PropertySheet, Rect,
    TreeRow, Widget, WidgetKind as Kind, Window, state,
};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

/// 系统墙纸目录；Windows 2000 的 %windir% 墙纸由导入器以原安装名放在这里。
const WALLPAPERS: &str = "/usr/share/liteos/wallpapers";
const NONE: &str = "(无)";
const STYLES: [(&str, WallpaperStyle); 3] = [
    ("居中", WallpaperStyle::Center),
    ("平铺", WallpaperStyle::Tile),
    ("拉伸", WallpaperStyle::Stretch),
];
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

/// 本页的领域值：墙纸、显示方式、背景色。
type Value = (Option<PathBuf>, WallpaperStyle, Option<[u8; 3]>);

/// 墙纸列表的行；行标识为下标加一，第一行恒为“(无)”。“浏览”追加的图片与系统墙纸同列。
#[derive(Clone)]
struct Choices {
    list: Widget,
    paths: Rc<RefCell<Vec<Option<PathBuf>>>>,
}

impl Choices {
    fn publish(&self) -> liteos_ui::Result<()> {
        let paths = self.paths.borrow();
        let names: Vec<String> = paths
            .iter()
            .map(|path| {
                path.as_deref()
                    .and_then(Path::file_stem)
                    .map_or_else(|| NONE.into(), |stem| stem.to_string_lossy().into_owned())
            })
            .collect();
        self.list.set_rows(
            &names
                .iter()
                .enumerate()
                .map(|(index, name)| (index as u64 + 1, name.as_str()))
                .collect::<Vec<_>>(),
        )
    }

    /// 选中 `path`；不在列表中则追加，与 Windows 2000 浏览后加入列表一致。
    fn choose(&self, path: Option<PathBuf>) -> liteos_ui::Result<()> {
        let index = {
            let mut paths = self.paths.borrow_mut();
            match paths.iter().position(|item| *item == path) {
                Some(index) => index,
                None => {
                    paths.push(path);
                    paths.len() - 1
                }
            }
        };
        self.publish()?;
        self.list.select(Some(index as u64 + 1))
    }

    fn selected(&self) -> liteos_ui::Result<Option<PathBuf>> {
        let index = self.list.selection()?.unwrap_or(1).saturating_sub(1) as usize;
        Ok(self.paths.borrow().get(index).cloned().flatten())
    }
}

/// 预览只依赖控件当前值；“浏览”任务与页面事件共用。
#[derive(Clone)]
struct Preview {
    canvas: Widget,
    choices: Choices,
    style: Widget,
    color: Widget,
}

impl Preview {
    fn value(&self) -> liteos_ui::Result<Value> {
        let style = STYLES
            .get(self.style.selection()?.unwrap_or(1).saturating_sub(1) as usize)
            .map_or(WallpaperStyle::Center, |(_, style)| *style);
        let color = COLORS
            .get(self.color.selection()?.unwrap_or(1).saturating_sub(1) as usize)
            .and_then(|(_, color)| *color);
        Ok((self.choices.selected()?, style, color))
    }

    /// 经典监视器：机壳、屏幕（背景色 + 墙纸）、底座。屏幕代表整个桌面。
    fn draw(&self) -> liteos_ui::Result<()> {
        let area = self.canvas.geometry()?;
        if area.width <= 0 || area.height <= 0 {
            return Ok(());
        }
        let (wallpaper, style, color) = self.value()?;
        let color = color.map_or(
            self.canvas.color(liteos_ui::ColorRole::Desktop)?,
            |[r, g, b]| u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b),
        );
        let path = wallpaper
            .as_deref()
            .and_then(Path::to_str)
            .unwrap_or_default();
        let x = (area.width - 140) / 2;
        let screen = Rect::new(x + 8, 12, 124, 72);
        let draw = |kind, rect, color, text| Draw {
            kind,
            rect,
            color,
            bold: false,
            text,
        };
        let mut commands = vec![
            draw(1, Rect::new(x, 4, 140, 95), 0xd4d0c8, ""),
            draw(2, Rect::new(x, 4, 140, 95), 0, ""),
            draw(1, screen, color, ""),
        ];
        if !path.is_empty() {
            let style = match style {
                WallpaperStyle::Center => 0,
                WallpaperStyle::Tile => 1,
                WallpaperStyle::Stretch => 2,
            };
            commands.push(draw(7, screen, style, path));
        }
        commands.extend([
            draw(1, Rect::new(x + 48, 99, 44, 8), 0x808080, ""),
            draw(1, Rect::new(x + 35, 107, 70, 6), 0xd4d0c8, ""),
        ]);
        self.canvas.set_drawing(&commands)
    }
}

pub struct Display {
    _controls: Vec<Widget>,
    preview: Preview,
    browse: Widget,
    dialogs: Dialogs,
    window: Window,
    saved: Value,
}

fn dropdown(
    parent: &Widget,
    items: &[&str],
    selected: usize,
    layout: Layout,
) -> liteos_ui::Result<Widget> {
    let combo = child(parent, Kind::Combo, items[selected], layout)?;
    combo.set_state(state::DROPDOWN_LIST)?;
    combo.set_tree_rows(
        &items
            .iter()
            .enumerate()
            .map(|(index, text)| TreeRow {
                id: index as u64 + 1,
                parent: 0,
                text,
                expanded: false,
                has_children: false,
            })
            .collect::<Vec<_>>(),
    )?;
    combo.select(Some(selected as u64 + 1))?;
    Ok(combo)
}

impl Display {
    pub fn build(
        sheet: &PropertySheet,
        settings: Settings,
        dialogs: Dialogs,
    ) -> liteos_ui::Result<Self> {
        let page = sheet.page("背景")?;
        let canvas = child(
            &page,
            Kind::Canvas,
            "",
            Layout {
                height: 120,
                ..Default::default()
            },
        )?;
        let body = child(
            &page,
            Kind::Row,
            "",
            Layout {
                grow: 1,
                gap: 8,
                ..Default::default()
            },
        )?;
        let left = child(
            &body,
            Kind::Column,
            "",
            Layout {
                grow: 1,
                gap: 4,
                ..Default::default()
            },
        )?;
        // shell32 原文是“选择背景图片或 HTML 文档作为墙纸”；LiteOS 不渲染 HTML 墙纸，不宣称该能力。
        let heading = label(&left, "选择背景图片作为墙纸(&S):")?;
        let list = child(
            &left,
            Kind::List,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let right = child(
            &body,
            Kind::Column,
            "",
            Layout {
                width: 100,
                gap: 6,
                ..Default::default()
            },
        )?;
        let spacer = child(&right, Kind::Label, "", Layout::default())?;
        let browse = child(
            &right,
            Kind::Button,
            "浏览(&B)...",
            Layout {
                height: 23,
                ..Default::default()
            },
        )?;
        let style_label = label(&right, "显示图片(&D):")?;
        let fixed = Layout {
            height: 21,
            ..Default::default()
        };
        let names: Vec<_> = STYLES.iter().map(|(name, _)| *name).collect();
        let selected = STYLES
            .iter()
            .position(|(_, style)| *style == settings.wallpaper_style)
            .unwrap_or(0);
        let style = dropdown(&right, &names, selected, fixed)?;
        let colors_row = child(
            &page,
            Kind::Row,
            "",
            Layout {
                height: 21,
                gap: 6,
                ..Default::default()
            },
        )?;
        let color_label = child(
            &colors_row,
            Kind::Label,
            "背景颜色(&C):",
            Layout {
                width: 90,
                ..Default::default()
            },
        )?;
        let names: Vec<_> = COLORS.iter().map(|(name, _)| *name).collect();
        let selected = COLORS
            .iter()
            .position(|(_, color)| *color == settings.background)
            .unwrap_or(0);
        let color = dropdown(
            &colors_row,
            &names,
            selected,
            Layout {
                width: 120,
                ..Default::default()
            },
        )?;
        let mut paths = vec![None];
        let mut system: Vec<PathBuf> = std::fs::read_dir(WALLPAPERS)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        system.sort();
        paths.extend(system.into_iter().map(Some));
        let choices = Choices {
            list,
            paths: Rc::new(RefCell::new(paths)),
        };
        choices.choose(settings.wallpaper.clone())?;
        let preview = Preview {
            canvas,
            choices,
            style,
            color,
        };
        preview.draw()?;
        Ok(Self {
            _controls: vec![
                page,
                body,
                left,
                heading,
                right,
                spacer,
                style_label,
                colors_row,
                color_label,
            ],
            preview,
            browse,
            dialogs,
            window: sheet.window().clone(),
            saved: (
                settings.wallpaper,
                settings.wallpaper_style,
                settings.background,
            ),
        })
    }
}

/// “浏览”：选择图片后加入列表并选中，与 Windows 2000 一致；取消不改变选择。
async fn browse(dialogs: Dialogs, window: Window, preview: Preview) {
    let directory = preview
        .choices
        .selected()
        .ok()
        .flatten()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(WALLPAPERS));
    let chosen = match dialogs.file_filtered(
        &window,
        FileDialogMode::Open,
        &directory,
        "",
        FileFilter {
            label: "图片 (*.bmp;*.jpg;*.jpeg;*.png)",
            extensions: &["bmp", "jpg", "jpeg", "png"],
        },
    ) {
        Ok(dialog) => dialog.await,
        Err(error) => Err(error),
    };
    match chosen {
        Ok(Some(path)) => {
            if let Err(error) = preview
                .choices
                .choose(Some(path))
                .and_then(|()| preview.draw())
            {
                eprintln!("control-panel: wallpaper: {error}");
            }
        }
        Ok(None) => {}
        Err(error) => eprintln!("control-panel: wallpaper browse: {error}"),
    }
}

impl Page for Display {
    fn event(&mut self, event: &Event) -> liteos_ui::Result<Option<Response>> {
        if matches!(event.kind, EventKind::Activate { .. }) && event.node == self.browse.id() {
            let task: Activity = Box::pin(browse(
                self.dialogs.clone(),
                self.window.clone(),
                self.preview.clone(),
            ));
            return Ok(Some(Response::Task(task)));
        }
        self.preview.draw()?;
        Ok(None)
    }
    fn dirty(&self) -> liteos_ui::Result<bool> {
        Ok(self.preview.value()? != self.saved)
    }
    fn apply(&self) -> liteos_ui::Result<Apply> {
        let (wallpaper, style, background) = self.preview.value()?;
        Ok(Box::new(move || {
            Settings::update(|settings| {
                settings.wallpaper = wallpaper;
                settings.wallpaper_style = style;
                settings.background = background;
            })
            .map(drop)
            .map_err(|error| error.to_string())
        }))
    }
    fn committed(&mut self) -> liteos_ui::Result<()> {
        self.saved = self.preview.value()?;
        Ok(())
    }
}
