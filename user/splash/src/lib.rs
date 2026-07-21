//! LiteOS boot splash and DRM-master handoff.

mod render;

use std::{fs, io, process, thread, time::Duration};

use linux_uapi::{
    drm::{Clip, DrmDevice, DumbBuffer},
    process::{self as linux_process, Fork},
};
use render::Canvas;

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let _ = run();
}

fn run() -> io::Result<()> {
    let device = DrmDevice::open("/dev/dri/card0")?;
    let topology = device.query_topology()?;
    let mut buffer =
        device.create_dumb(topology.mode.width().into(), topology.mode.height().into())?;
    let framebuffer_id = device.add_framebuffer(&buffer, 24)?;
    let mut canvas = unsafe {
        Canvas::new(
            buffer.as_mut_ptr(),
            buffer.pitch(),
            buffer.width(),
            buffer.height(),
        )
    };
    canvas.fill(0);
    if let Ok(logo) = fs::read("/usr/share/liteos/bootlogo.xrgb") {
        canvas.draw_bootlogo(&logo);
    }
    let track = canvas.track_origin();
    canvas.draw_track(track.0, track.1);
    device.set_crtc(&topology, framebuffer_id)?;
    let _ = device.dirty(framebuffer_id, &[]);
    device.drop_master()?;
    match linux_process::fork_background()? {
        Fork::Parent { .. } => process::exit(0),
        Fork::Child => {
            let _ = fs::write("/run/splash.pid", format!("{}\n", process::id()));
            animate(device, framebuffer_id, buffer, &mut canvas, track)
        }
    }
}

fn animate(
    device: DrmDevice,
    framebuffer_id: u32,
    _buffer: DumbBuffer,
    canvas: &mut Canvas,
    track: (usize, usize),
) -> ! {
    let mut offset = 0usize;
    loop {
        canvas.draw_sliders(track.0, track.1, offset);
        let clip = Clip {
            x1: track.0 as u16,
            y1: track.1 as u16,
            x2: (track.0 + render::TRACK_WIDTH) as u16,
            y2: (track.1 + render::TRACK_HEIGHT) as u16,
        };
        let _ = device.dirty(framebuffer_id, &[clip]);
        thread::sleep(Duration::from_millis(100));
        offset += render::SLIDER_STEP;
        if offset > render::max_slider_offset() {
            offset = 0;
        }
    }
}
