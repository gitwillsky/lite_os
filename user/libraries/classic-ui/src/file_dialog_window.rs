//! 独立窗口里的文件对话框：Windows 2000 的通用对话框是单独的顶层窗口，可以拖动，不受调用者窗口大小限制。
//!
//! 窗口是固定尺寸并以调用者窗口为父窗口，合成器据此把它居中在父窗口上。窗口自己持有字体与绘制，
//! 调用者只需把属于它的事件交给 [`FileDialogWindow::event`]，并在结果为 `Chosen`/`Cancelled` 时销毁。

use std::{io, path::Path};

use canvas::{Rect, Surface};
use classic_theme::{SystemColor as C, Theme};
use typeface::Typeface;

use crate::{
    Decoration, DecorationAction, Display, Event, EventKind, FileDialog, FileDialogEvent,
    FrameKind, Painter, WindowId,
};

/// 左键的 Linux `BTN_LEFT`。
const BUTTON_LEFT: u32 = 0x110;
const FONT_DIRECTORY: &str = "/usr/share/liteos/fonts";

pub struct FileDialogWindow {
    window: WindowId,
    decoration: Decoration,
    dialog: FileDialog,
    theme: Theme,
    /// 字体按窗口的整数 scale 载入；`scale` 变化时重新载入。
    font: Option<Typeface>,
    scale: i32,
    pointer: (i32, i32),
    /// 本窗口尚未提交的失效；跨越在途 frame 保留，否则点击或 configure 会丢失重绘。
    dirty: bool,
}

impl FileDialogWindow {
    /// 打开对话框窗口。
    ///
    /// # Parameters
    ///
    /// - `app_id`: 窗口的 app_id（任务栏据此给图标）。
    /// - `parent`: 调用者的主窗口；对话框居中在它上面。
    /// - `dialog`: 已建立的对话框；本函数把它切换为独立窗口版面。
    ///
    /// # Errors
    ///
    /// 建窗口或设置固定尺寸、父窗口的 Wayland 请求失败。
    pub fn open(
        display: &mut Display,
        app_id: &str,
        parent: WindowId,
        dialog: FileDialog,
    ) -> io::Result<Self> {
        let dialog = dialog.windowed();
        let window = display.create_window(dialog.title(), app_id, FileDialog::WINDOW_SIZE)?;
        display.fix_size(window)?;
        display.set_parent(window, parent)?;
        Ok(Self {
            window,
            decoration: Decoration::new(FrameKind::Fixed),
            dialog,
            theme: Theme::classic(),
            font: None,
            scale: 0,
            pointer: (-1, -1),
            dirty: true,
        })
    }

    /// 这个对话框的窗口；调用者用它判断一个事件是否属于对话框。
    pub fn window(&self) -> WindowId {
        self.window
    }

    /// 处理属于本窗口的事件并重绘。
    ///
    /// # Returns
    ///
    /// 对话框结果；`Chosen`/`Cancelled` 之后调用者应调用 [`Self::close`]。
    ///
    /// # Errors
    ///
    /// 字体载入或 Wayland 请求失败。
    pub fn event(&mut self, display: &mut Display, event: &Event) -> io::Result<FileDialogEvent> {
        let Some(c) = display.configuration(self.window) else {
            return Ok(FileDialogEvent::Pending);
        };
        let client = self.decoration.client_rect(c);
        match self.decoration.event(&event.kind, c) {
            Some(DecorationAction::Request(request)) => display.request(self.window, request)?,
            Some(DecorationAction::Close) => return Ok(FileDialogEvent::Cancelled),
            Some(DecorationAction::Cursor(shape)) => display.set_cursor_shape(shape)?,
            None => {}
        }
        let mut result = FileDialogEvent::Pending;
        match &event.kind {
            EventKind::Close => return Ok(FileDialogEvent::Cancelled),
            EventKind::Configure(c) if self.scale != c.scale => {
                self.font = Some(
                    Typeface::load(Path::new(FONT_DIRECTORY), c.scale as u32)
                        .map_err(|error| io::Error::other(error.to_string()))?,
                );
                self.scale = c.scale;
            }
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                self.pointer = (x.floor() as i32, y.floor() as i32);
                self.dialog.motion(client, self.pointer);
            }
            EventKind::PointerLeave => self.pointer = (-1, -1),
            EventKind::Button {
                code: BUTTON_LEFT,
                pressed: true,
                ..
            } => result = self.dialog.press(client, self.pointer),
            EventKind::Button {
                code: BUTTON_LEFT,
                pressed: false,
                ..
            } => self.dialog.release(),
            EventKind::Wheel { vertical, .. } => self.dialog.wheel(client, *vertical),
            EventKind::Key(key) if key.pressed => result = self.dialog.key(*key),
            _ => {}
        }
        if result == FileDialogEvent::Pending {
            self.dirty |= !matches!(event.kind, EventKind::FramePresented { .. });
            self.paint(display)?;
        }
        Ok(result)
    }

    fn paint(&mut self, display: &mut Display) -> io::Result<()> {
        let (Some(c), Some(font)) = (display.configuration(self.window), self.font.as_ref()) else {
            return Ok(());
        };
        if !self.dirty || !display.ready_to_draw(self.window) {
            return Ok(());
        }
        let (width, height) = c.pixel_size()?;
        let mut pixels = vec![0; width as usize * height as usize];
        let surface = Surface::new(&mut pixels, width, height, width as usize)
            .ok_or_else(|| io::Error::other("invalid file dialog surface"))?;
        let mut painter = Painter::new(surface, &self.theme, font, c.scale);
        painter.fill(Rect::new(0, 0, c.width, c.height), C::ButtonFace);
        let client = self.decoration.client_rect(c);
        self.decoration.paint(&mut painter, c, self.dialog.title());
        self.dialog.paint(&mut painter, client);
        display.present(self.window, &pixels)?;
        self.dirty = false;
        Ok(())
    }

    /// 销毁窗口。
    ///
    /// # Errors
    ///
    /// Wayland 请求失败。
    pub fn close(self, display: &mut Display) -> io::Result<()> {
        display.destroy_window(self.window)
    }
}
