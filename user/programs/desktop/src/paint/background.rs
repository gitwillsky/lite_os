//! 桌面根只提交一像素底色；不可变墙纸、透明图标与反色选框边缘用同步子表面原子呈现。
use super::*;
use classic_ui::ImageId;

#[derive(Default)]
pub(super) struct Scene {
    image: Option<ImageId>,
    quilt: Option<ImageId>,
    tiles: Vec<WindowId>,
    /// 已同步提交 null attach，等待本次父 commit 后再销毁协议角色。
    retiring: Vec<WindowId>,
    layout: Option<(i32, i32, desktop_settings::WallpaperStyle, ImageId)>,
    icons: Option<Overlay>,
    edges: [Option<Overlay>; 4],
}
struct Overlay {
    window: WindowId,
    bounds: Rect,
    covered: Vec<Rect>,
}
#[derive(Clone, Copy)]
enum OverlayKind {
    Icons,
    Selection(Option<ImageId>),
}
fn cover(a: Rect, b: Rect) -> Rect {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect::new(
        x,
        y,
        a.right().max(b.right()) - x,
        a.bottom().max(b.bottom()) - y,
    )
}
fn shifted(rect: Rect, origin: Rect) -> Rect {
    Rect::new(
        rect.x - origin.x,
        rect.y - origin.y,
        rect.width,
        rect.height,
    )
}
fn delta(s: &Desktop) -> (i32, i32) {
    match &s.drag {
        Some(crate::Drag::Icons {
            index,
            grab,
            pos,
            moved: true,
            ..
        }) => {
            let origin = s.icons.rect(*index);
            (pos.0 - grab.0 - origin.x, pos.1 - grab.1 - origin.y)
        }
        _ => (0, 0),
    }
}
fn icon_rect(s: &Desktop, index: usize) -> Rect {
    let rect = s.icons.rect(index);
    let delta = if s.selection.binary_search(&index).is_ok() {
        delta(s)
    } else {
        (0, 0)
    };
    Rect::new(rect.x + delta.0, rect.y + delta.1, rect.width, rect.height)
}
fn icon_selected(s: &Desktop, index: usize) -> bool {
    s.selection.binary_search(&index).is_ok()
        || ((s.drop_hover || matches!(s.drag, Some(crate::Drag::Icons { recycle: true, .. })))
            && s.icons.items[index].target
                == crate::icons::Target::Place(crate::start::Place::RecycleBin))
}
fn icon_bounds(s: &Desktop, c: Configuration, art: &mut Art, focused: bool) -> io::Result<Rect> {
    let fonts = art.resources.fonts(c.scale)?;
    let mut scratch = [0];
    let mut probe = Painter::new(
        Surface::new(&mut scratch, 1, 1, 1).expect("icon bounds probe"),
        art.resources.theme(),
        &fonts,
        c.scale,
    );
    let mut bounds = Rect::default();
    for (index, icon) in s.icons.items.iter().enumerate() {
        let rect = icon_rect(s, index);
        let bitmap = art.desktop_icon_bitmap(icon, s.icons.trash_full);
        let (width, height) = bitmap.map_or((32, 32), |bitmap| (bitmap.width(), bitmap.height()));
        if bitmap.is_some() {
            bounds = cover(
                bounds,
                classic_ui::large_icon_image_rect(rect, width, height),
            );
        }
        let editing = s.rename.as_ref().is_some_and(|rename| matches!(&icon.target, crate::icons::Target::Path(path) if path == rename.operation.path()));
        let label = if editing {
            classic_ui::large_icon_edit_rect(rect, height)
        } else {
            probe.with_role(FontRole::Icon, |p| {
                p.icon_label(
                    rect,
                    height,
                    &icon.label,
                    focused && s.focused == Some(index) && icon_selected(s, index),
                )
                .rect
            })
        };
        bounds = cover(bounds, label);
    }
    if !s.message.is_empty() {
        bounds = cover(
            bounds,
            Rect::new(100, 40, (c.width - 120).max(0), probe.line_height()),
        );
    }
    Ok(bounds.intersect(Rect::new(0, 0, c.width, c.height)))
}
fn ensure_overlay(
    slot: &mut Option<Overlay>,
    display: &mut Display,
    parent: WindowId,
    bounds: Rect,
    retiring: &mut Vec<WindowId>,
) -> io::Result<()> {
    if bounds.is_empty() {
        if let Some(overlay) = slot.take() {
            display.unmap_surface(overlay.window)?;
            retiring.push(overlay.window);
        }
    } else if slot.is_none() {
        let window = display.create_subsurface(parent, bounds)?;
        display.set_input_region(window, Some(Rect::default()))?;
        *slot = Some(Overlay {
            window,
            bounds,
            covered: Vec::new(),
        });
    }
    Ok(())
}
impl Scene {
    pub(super) fn replace_image(
        &mut self,
        display: &mut Display,
        bitmap: Option<&Bitmap>,
    ) -> io::Result<()> {
        let next = bitmap
            .map(|bitmap| display.create_image(bitmap))
            .transpose()?;
        if let Some(image) = self.quilt.take() {
            display.destroy_image(image)?;
        }
        if let Some(image) = self.image.take() {
            display.destroy_image(image)?;
        }
        self.image = next;
        self.layout = None;
        Ok(())
    }
    /// 小图案先生成有界重复块，再共享给所有瓦片，避免创建几万/几百万个 surface。
    /// 只增加至多 64K 像素的不可变缓存；块尺寸是原图的整数倍，平铺相位和每个像素都不变。
    fn tiled_image(&mut self, display: &mut Display, image: ImageId) -> io::Result<ImageId> {
        if let Some(quilt) = self.quilt {
            return Ok(quilt);
        }
        let source = display.image_view(image)?;
        let (width, height) = (source.width as usize, source.height as usize);
        let budget = (width * height).max(65536);
        let horizontal = (256 / width).max(1).min(budget / (width * height));
        let vertical = (256 / height)
            .max(1)
            .min(budget / (width * height * horizontal));
        if horizontal == 1 && vertical == 1 {
            return Ok(image);
        }
        let (w, h) = (width * horizontal, height * vertical);
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(w * h).map_err(io::Error::other)?;
        for row in 0..h {
            let start = (row % height) * source.stride;
            for _ in 0..horizontal {
                pixels.extend_from_slice(&source.pixels[start..start + width]);
            }
        }
        let bitmap = Bitmap::from_pixels(pixels, w as i32, h as i32);
        let quilt = display.create_image(&bitmap)?;
        self.quilt = Some(quilt);
        Ok(quilt)
    }
    fn wallpaper(
        &mut self,
        display: &mut Display,
        root: WindowId,
        c: Configuration,
        style: desktop_settings::WallpaperStyle,
    ) -> io::Result<()> {
        let Some(original) = self.image else {
            for tile in self.tiles.drain(..) {
                display.unmap_surface(tile)?;
                self.retiring.push(tile);
            }
            return Ok(());
        };
        let image = if style == desktop_settings::WallpaperStyle::Tile {
            self.tiled_image(display, original)?
        } else {
            original
        };
        let key = (c.width, c.height, style, image);
        if self.layout == Some(key) {
            return Ok(());
        }
        let (w, h) = display.image_size(image)?;
        let rectangles = match style {
            desktop_settings::WallpaperStyle::Center => {
                vec![Rect::new((c.width - w) / 2, (c.height - h) / 2, w, h)]
            }
            desktop_settings::WallpaperStyle::Stretch => vec![Rect::new(0, 0, c.width, c.height)],
            desktop_settings::WallpaperStyle::Tile => (0..c.height)
                .step_by(h as usize)
                .flat_map(|y| {
                    (0..c.width)
                        .step_by(w as usize)
                        .map(move |x| Rect::new(x, y, w, h))
                })
                .collect(),
        };
        while self.tiles.len() > rectangles.len() {
            let tile = self.tiles.pop().expect("excess tile");
            display.unmap_surface(tile)?;
            self.retiring.push(tile);
        }
        for (index, bounds) in rectangles.into_iter().enumerate() {
            let window = match self.tiles.get(index) {
                Some(window) => *window,
                None => {
                    let window = display.create_subsurface(root, bounds)?;
                    display.set_input_region(window, Some(Rect::default()))?;
                    self.tiles.push(window);
                    window
                }
            };
            display.set_subsurface_bounds(window, bounds)?;
            display.place_subsurface_above(window, root)?;
            display.present_image(window, image)?;
        }
        self.layout = Some(key);
        Ok(())
    }
    fn present(&mut self, s: &Desktop, display: &mut Display, art: &mut Art) -> io::Result<bool> {
        // 根 callback 到达前不进入这里；子 callback 与 release 也必须都允许重写，不能覆盖忙缓冲。
        if self
            .icons
            .iter()
            .chain(self.edges.iter().flatten())
            .any(|overlay| !display.ready_to_draw(overlay.window))
        {
            return Ok(false);
        }
        let c = display
            .configuration(s.background)
            .ok_or(io::ErrorKind::NotFound)?;
        self.wallpaper(display, s.background, c, s.settings.wallpaper_style)?;
        let bounds = icon_bounds(s, c, art, display.keyboard_focus() == Some(s.background))?;
        ensure_overlay(
            &mut self.icons,
            display,
            s.background,
            bounds,
            &mut self.retiring,
        )?;
        let screen = Rect::new(0, 0, c.width, c.height);
        let selection = s
            .drag
            .as_ref()
            .and_then(crate::Drag::selection_rect)
            .map(|rect| rect.intersect(screen))
            .filter(|rect| !rect.is_empty());
        let edges = selection.map_or_else(
            || [Rect::default(); 4],
            |rect| {
                [
                    Rect::new(rect.x, rect.y, rect.width, 1),
                    Rect::new(rect.x, rect.bottom() - 1, rect.width, 1),
                    Rect::new(rect.x, rect.y, 1, rect.height),
                    Rect::new(rect.right() - 1, rect.y, 1, rect.height),
                ]
            },
        );
        let mut above = self.tiles.first().copied().unwrap_or(s.background);
        if let Some(icons) = &mut self.icons {
            display.place_subsurface_above(
                icons.window,
                self.tiles.first().copied().unwrap_or(s.background),
            )?;
            icons.present(s, display, art, bounds, OverlayKind::Icons)?;
            above = icons.window;
        }
        for (slot, edge) in self.edges.iter_mut().zip(edges) {
            ensure_overlay(slot, display, s.background, edge, &mut self.retiring)?;
            if let Some(overlay) = slot {
                display.place_subsurface_above(overlay.window, above)?;
                overlay.present(s, display, art, edge, OverlayKind::Selection(self.image))?;
                above = overlay.window;
            }
        }
        let color = s.settings.background.map_or_else(
            || art.resources.theme().color(C::Background),
            |[red, green, blue]| classic_theme::Rgb { red, green, blue },
        );
        display.present_solid(
            s.background,
            canvas::Color::rgb(color.red, color.green, color.blue).0,
        )?;
        // null attach 先随父提交生效，随后销毁不会让旧/新背景之间出现非原子的空白帧。
        for surface in self.retiring.drain(..) {
            display.destroy_window(surface)?;
        }
        Ok(true)
    }
}
impl Art {
    pub fn present_background(&mut self, s: &Desktop, display: &mut Display) -> io::Result<bool> {
        let mut scene = std::mem::take(&mut self.backdrop);
        let result = scene.present(s, display, self);
        self.backdrop = scene;
        result
    }
}

impl Overlay {
    fn present(
        &mut self,
        s: &Desktop,
        display: &mut Display,
        art: &mut Art,
        bounds: Rect,
        kind: OverlayKind,
    ) -> io::Result<()> {
        let moved = self.bounds != bounds;
        display.set_subsurface_bounds(self.window, bounds)?;
        let mut covered = std::mem::take(&mut self.covered);
        let result = display.paint_frame(self.window, |pixels, c, preserved, d| {
            let root = d.configuration(s.background).ok_or(io::ErrorKind::NotFound)?;
            let screen = Rect::new(0, 0, root.width, root.height);
            let (w, h) = c.pixel_size()?;
            let mut damage = if preserved && !moved { std::mem::take(&mut covered) } else { vec![Rect::new(0, 0, bounds.width, bounds.height)] };
            let mut surface = Surface::new(pixels, w, h, w as usize).ok_or(io::ErrorKind::InvalidInput)?;
            for rect in &damage { surface.fill_rect(Rect::new(rect.x * c.scale, rect.y * c.scale, rect.width * c.scale, rect.height * c.scale), canvas::Color(0)); }
            let fonts = art.resources.fonts(c.scale)?;
            let theme = art.resources.theme();
            let mut p = Painter::new(surface, theme, &fonts, c.scale);
            p.set_brush_origin((bounds.x, bounds.y));
            if let OverlayKind::Selection(image) = kind {
                // 反色边缘必须基于实际底色/墙纸/图标；只光栅窄边，不复制整屏背景。
                let color = s.settings.background.map_or_else(|| theme.color(C::Background), |[red, green, blue]| classic_theme::Rgb { red, green, blue });
                let local = Rect::new(0, 0, bounds.width, bounds.height);
                p.fill_rgb(local, color);
                if let Some(image) = image {
                    let style = match s.settings.wallpaper_style { desktop_settings::WallpaperStyle::Center => classic_ui::WallpaperStyle::Center,
                        desktop_settings::WallpaperStyle::Tile => classic_ui::WallpaperStyle::Tile, desktop_settings::WallpaperStyle::Stretch => classic_ui::WallpaperStyle::Stretch };
                    p.wallpaper(d.image_view(image)?, shifted(screen, bounds), style, (root.width, root.width));
                }
                covered.push(local);
            }
            let focused = d.keyboard_focus() == Some(s.background);
            for (i, icon) in s.icons.items.iter().enumerate() {
                let r = shifted(icon_rect(s, i), bounds);
                let selected = icon_selected(s, i);
        let bitmap = art.desktop_icon_bitmap(icon, s.icons.trash_full);
        let editing = s.rename.as_ref().filter(|rename| {
            matches!(&icon.target, crate::icons::Target::Path(path) if path == rename.operation.path())
        });
        let (width, height) = bitmap.map_or((32, 32), |bitmap| (bitmap.width(), bitmap.height()));
        if bitmap.is_some() {
            covered
                .push(classic_ui::large_icon_image_rect(r, width, height));
        }
        let label = p.with_role(FontRole::Icon, |p| {
            let label = p.large_icon_item(
                r,
                bitmap,
                editing.is_none().then_some(&icon.label),
                classic_ui::ItemLook {
                    selected,
                    active: true,
                    focused: focused && selected && s.focused == Some(i),
                    text: C::HilightText,
                },
            );
            if let Some(rename) = editing {
                let edit = classic_ui::large_icon_edit_rect(r, height);
                p.edit(edit, &rename.edit, focused, false);
                edit
            } else {
                label
            }
        });
        if !label.is_empty() {
            covered.push(label);
        }
    }

            if matches!(kind, OverlayKind::Selection(_)) && let Some(area) = s.drag.as_ref().and_then(crate::Drag::selection_rect).map(|rect| rect.intersect(screen)) {
                p.focus_rect(shifted(area, bounds));
            }
            if !s.message.is_empty() {
                let message = Rect::new(100 - bounds.x, 40 - bounds.y, (root.width - 120).max(0), p.line_height());
                p.text(message.x, message.y, &p.fit(&s.message, message.width), Ink::Color(C::HilightText), false);
                covered.push(message);
            }
            let local = Rect::new(0, 0, bounds.width, bounds.height);
            for rect in &mut covered { *rect = rect.intersect(local); }
            covered.retain(|rect| !rect.is_empty());
            damage.extend(covered.iter().copied());
            Ok(damage.into_iter().map(|rect| rect.intersect(local)).filter(|rect| !rect.is_empty()).map(|rect|
                Rect::new(rect.x * c.scale, rect.y * c.scale, rect.width * c.scale, rect.height * c.scale)).collect())
        });
        self.covered = covered;
        if result.is_ok() {
            self.bounds = bounds;
        }
        result
    }
}
