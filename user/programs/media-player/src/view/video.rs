use super::{Engine, LOGO, Rect, View};
use liteos_ui::{Draw, VideoBuffer};

#[derive(Default)]
pub(super) struct VideoFrames {
    playing: Option<bool>,
    // A newer complete frame replaces one that was waiting for a frame callback.
    pending: Option<mpv_client::Frame>,
    submitted: [Option<mpv_client::Frame>; mpv_client::FRAME_SLOTS],
}

impl View {
    /// 视频画布的设备像素尺寸：逻辑矩形乘以窗口 scale。
    pub fn canvas_pixels(&self) -> liteos_ui::Result<Option<(u32, u32)>> {
        let area = self.video.geometry()?;
        let scale = self.scale.get().max(1);
        Ok((area.width > 0 && area.height > 0)
            .then(|| ((area.width * scale) as u32, (area.height * scale) as u32)))
    }

    /// 渲染当前帧；空闲时清除旧帧，在黑底上居中显示 Windows Media 标志（WMP 6.4 未打开媒体时的画面）。
    pub fn render(&self, engine: &mut Engine) -> liteos_ui::Result<()> {
        let mut frames = self.frames.borrow_mut();
        let Some((width, height)) = self.canvas_pixels()? else {
            return Ok(());
        };
        if engine.state.idle {
            if frames.playing != Some(false) {
                self.video.present_video(None)?;
                if let Some(frame) = frames.pending.take() {
                    engine.recycle_frame(frame);
                }
                engine.clear_render_size();
                frames.playing = Some(false);
            }
            let area = self.video.geometry()?;
            let rect = Rect::new(0, 0, area.width, area.height);
            return self.video.set_drawing(&[
                Draw {
                    kind: 1,
                    rect,
                    color: 0,
                    bold: false,
                    text: "",
                },
                Draw {
                    kind: 8,
                    rect,
                    color: 0,
                    bold: false,
                    text: LOGO,
                },
            ]);
        }
        if frames.playing != Some(true) {
            self.video.set_drawing(&[])?;
            frames.playing = Some(true);
        }
        match engine.render(width, height) {
            Ok(Some(frame)) => {
                if let Some(old) = frames.pending.replace(frame) {
                    engine.recycle_frame(old);
                }
            }
            Ok(None) => {}
            Err(error) => {
                engine.state.message = format!("渲染失败：{error}");
                return self.controls_state(&engine.state);
            }
        }
        if let Some(frame) = frames.pending.as_ref() {
            if frame.size() != (width, height) {
                engine.recycle_frame(frames.pending.take().expect("pending old-size frame"));
            } else {
                let slot = frame.slot();
                let result = self.video.present_video(Some(VideoBuffer {
                    fd: frame.as_fd(),
                    slot,
                    generation: frame.generation(),
                    width: width as i32,
                    height: height as i32,
                    stride: frame.stride() as i32,
                }));
                match result {
                    Ok(false) => {}
                    Ok(true) => {
                        debug_assert!(frames.submitted[slot as usize].is_none());
                        frames.submitted[slot as usize] = frames.pending.take();
                    }
                    Err(error) => {
                        // A partial socket write may have published the slot. Stop the window
                        // before a later render could recycle its storage.
                        let _ = self.window.close();
                        return Err(error);
                    }
                }
            }
        }
        Ok(())
    }

    /// Only the compositor's release event can return a submitted slot to mpv.
    pub fn release_video(&self, engine: &mut Engine, slot: u32) {
        if let Some(frame) = self
            .frames
            .borrow_mut()
            .submitted
            .get_mut(slot as usize)
            .and_then(Option::take)
        {
            engine.recycle_frame(frame);
        }
    }
}
