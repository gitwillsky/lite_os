//! Canvas 视频子表面的窗口几何与应用事件由 UI context 独占。

use super::{
    Context,
    widgets::{Tree, invalid},
};
use canvas::Rect;
use classic_ui::{Display, EventKind, WindowHost, WindowId};
use classic_ui_abi::{self as abi, VideoFrame};
use std::{io, os::fd::BorrowedFd};

pub(super) struct Video {
    pub(super) node: u64,
    pub(super) surface: WindowId,
    pub(super) visible: bool,
    pub(super) extent: (i32, i32),
    pub(super) area: Rect,
}

pub(super) fn bounds(tree: &Tree, id: u64) -> Rect {
    tree.nodes
        .get(&id)
        .filter(|node| node.kind == abi::widget::CANVAS && node.visible())
        .map_or(Rect::new(0, 0, 0, 0), |node| node.rect.intersect(node.clip))
}

pub(super) fn fitted(area: Rect, (width, height): (i32, i32)) -> Rect {
    let (source_w, source_h) = (i64::from(width), i64::from(height));
    let (w, h) = (i64::from(area.width), i64::from(area.height));
    let (width, height) = if w * source_h <= h * source_w {
        (area.width, (w * source_h / source_w).max(1) as i32)
    } else {
        ((h * source_w / source_h).max(1) as i32, area.height)
    };
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

pub(super) fn present(
    context: &mut Context,
    window: u64,
    id: u64,
    frame: Option<(&VideoFrame, BorrowedFd<'_>)>,
) -> io::Result<()> {
    context.layout(window)?;
    let host = context
        .windows
        .get_mut(&window)
        .ok_or(io::ErrorKind::NotFound)?;
    let tree = match &host.body {
        super::context::WindowBody::Widgets(tree) => tree,
        _ => return Err(invalid("video requires a canvas in a normal window")),
    };
    if !tree
        .nodes
        .get(&id)
        .is_some_and(|node| node.kind == abi::widget::CANVAS)
        || host.video.as_ref().is_some_and(|video| video.node != id)
    {
        return Err(invalid("window already owns a different video canvas"));
    }
    let Some((frame, fd)) = frame else {
        if let Some(video) = &mut host.video
            && video.visible
        {
            context.display.hide_stream(video.surface)?;
            video.visible = false;
            host.frame.set_transparent(&mut context.display, false)?;
        }
        return Ok(());
    };
    let area = bounds(tree, id);
    if area.is_empty() {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    if frame.fd < 0 || frame.generation == 0 || frame.width <= 0 || frame.height <= 0 {
        return Err(invalid("invalid shared video frame"));
    }
    let extent = (frame.width, frame.height);
    let area = fitted(area, extent);
    if host.video.is_none() {
        let surface = context.display.create_stream(host.frame.window(), area)?;
        host.video = Some(Video {
            node: id,
            surface,
            visible: false,
            extent,
            area,
        });
    }
    let video = host.video.as_mut().expect("stream created");
    if !context.display.ready_to_draw(video.surface) {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    let geometry_changed = context.display.set_stream_bounds(video.surface, area)?;
    if geometry_changed {
        host.frame.invalidate_frame();
    }
    if !context.display.submit_stream(
        video.surface,
        frame.slot,
        frame.generation,
        fd,
        [frame.width, frame.height, frame.stride],
    )? {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    if !video.visible {
        video.visible = true;
        host.frame.set_transparent(&mut context.display, true)?;
    }
    video.extent = extent;
    video.area = area;
    Ok(())
}

pub(super) fn event(
    host: &super::context::WindowRecord,
    window: u64,
    input: &EventKind,
) -> Option<abi::Event> {
    let video = host.video.as_ref()?;
    let (kind, value) = match input {
        EventKind::StreamBufferReleased(slot) => (abi::event::VIDEO_RELEASED, u64::from(*slot)),
        EventKind::StreamFrameReady => (abi::event::VIDEO_READY, 0),
        _ => return None,
    };
    Some(abi::Event {
        window,
        node: video.node,
        kind,
        value,
        ..Default::default()
    })
}

pub(super) fn layout(
    video: &mut Option<Video>,
    host: &mut WindowHost,
    display: &mut Display,
    tree: &Tree,
) -> io::Result<()> {
    if let Some(video) = video.as_mut().filter(|video| video.visible) {
        let area = bounds(tree, video.node);
        if area.is_empty() {
            display.hide_stream(video.surface)?;
            video.visible = false;
            host.set_transparent(display, false)?;
        } else {
            let area = fitted(area, video.extent);
            if display.set_stream_bounds(video.surface, area)? {
                video.area = area;
                host.invalidate_frame();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_frame_keeps_its_aspect_during_resize() {
        let area = Rect::new(8, 12, 100, 100);
        assert_eq!(fitted(area, (1920, 1080)), Rect::new(8, 34, 100, 56));
        assert_eq!(fitted(area, (90, 180)), Rect::new(33, 12, 50, 100));
    }
}
