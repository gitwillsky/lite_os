//! 客户端 set_cursor 请求与硬件光标呈现；光标图像与位置仍由 Session 唯一持有。

use super::*;

impl Session {
    /// 客户端的 `set_cursor`：只接受当前指针焦点客户端、带最近一次 enter 序号的请求。
    pub(super) fn handle_cursor_requests(&mut self) -> io::Result<()> {
        let focus = self.seat.pointer_focus();
        if focus != self.last_pointer_focus {
            // 指针换了窗口：新窗口的光标由它自己在 enter 之后设置，在此之前用默认光标。
            self.last_pointer_focus = focus;
            if self.grab.is_none() {
                self.shape = Shape::Default;
                self.apply_cursor()?;
            }
        }
        let enter_serial = self.seat.pointer_enter_serial();
        for (client, request) in self.server.take_cursor_requests() {
            let current =
                focus.is_some_and(|(owner, _)| owner == client) && request.serial == enter_serial;
            if !current || self.grab.is_some() {
                continue;
            }
            self.shape = match (request.named, request.surface) {
                (Some(name), _) => Shape::Named(name),
                (None, Some(surface)) => Shape::Client {
                    client,
                    surface,
                    hotspot: request.hotspot,
                    offset_origin: request.offset_origin,
                },
                (None, None) => Shape::Hidden,
            };
            self.apply_cursor()?;
        }
        // 客户端光标 surface 重新提交了图像（动画光标、延迟提交的第一帧）。
        if matches!(self.shape, Shape::Client { .. }) && self.dirty {
            self.apply_cursor()?;
        }
        Ok(())
    }

    /// 把当前光标形状写进硬件光标。
    pub(super) fn apply_cursor(&mut self) -> io::Result<()> {
        let position = self.input.pointer().position();
        let scale = self.scale;
        match self.shape {
            Shape::Hidden => {
                self.applied_client_cursor = None;
                return self.scanout.hide_cursor(position);
            }
            Shape::Client {
                client,
                surface,
                hotspot,
                offset_origin,
            } => {
                match self.server.cursor_image(client, surface) {
                    // 无 buffer 的光标 surface 不可见；后续 commit 再更新硬件图像。
                    None => {
                        self.applied_client_cursor = None;
                        return self.scanout.hide_cursor(position);
                    }
                    Some(content) => {
                        let (width, height) = content.output_size(scale as i32);
                        let physical = |hot: i32, offset: i64, origin: i64| {
                            i64::from(hot)
                                .checked_sub(offset.checked_sub(origin)?)?
                                .checked_mul(i64::from(scale))
                                .and_then(|value| i32::try_from(value).ok())
                        };
                        let hot = (
                            physical(hotspot.0, content.offset.0, offset_origin.0),
                            physical(hotspot.1, content.offset.1, offset_origin.1),
                        );
                        if width <= cursor::HARDWARE_EDGE as i32
                            && height <= cursor::HARDWARE_EDGE as i32
                            && matches!(hot, (Some(x), Some(y)) if (0..64).contains(&x) && (0..64).contains(&y))
                        {
                            let physical_hot = (hot.0.unwrap() as u32, hot.1.unwrap() as u32);
                            if self.applied_client_cursor.as_ref().is_some_and(
                                |(image, previous)| {
                                    *previous == physical_hot && image.same_image(content)
                                },
                            ) {
                                return Ok(());
                            }
                            let edge = cursor::HARDWARE_EDGE as i32;
                            let stride = self.hardware_cursor.stride();
                            self.hardware_cursor.pixels_mut().fill(0);
                            let mut target = canvas::Surface::new(
                                self.hardware_cursor.pixels_mut(),
                                edge,
                                edge,
                                stride,
                            )
                            .expect("hardware cursor geometry");
                            target.blit_sampled(
                                canvas::Image {
                                    pixels: content.pixels()?,
                                    width: content.width as i32,
                                    height: content.height as i32,
                                    stride: content.width,
                                },
                                canvas::Rect::new(0, 0, width, height),
                                content.source,
                            );
                            let hot = (hot.0.unwrap() as u32, hot.1.unwrap() as u32);
                            self.scanout
                                .show_cursor(&self.hardware_cursor, position, hot)?;
                            self.applied_client_cursor = Some((content.clone(), hot));
                            return Ok(());
                        }
                        // 硬件光标最大 64×64；更大的图像退回默认光标。
                    }
                }
            }
            Shape::Default | Shape::Named(_) => {}
        }
        self.applied_client_cursor = None;
        let name = match self.shape {
            Shape::Named(name) => name,
            _ => "arrow",
        };
        let image = self
            .cursors
            .get(name)
            .or_else(|| self.cursors.get("arrow"))
            .ok_or_else(|| io::Error::other("cursor set has no `arrow`"))?;
        let (edge, frame) = image.frame(scale, 0);
        cursor::blit_to_hardware(&mut self.hardware_cursor, edge, frame)?;
        self.scanout
            .show_cursor(&self.hardware_cursor, position, image.hotspot(scale))
    }
}
