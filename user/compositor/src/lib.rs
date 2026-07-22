//! LiteOS graphical compositor and display-session owner.
//!
//! The process owns DRM, scanout, the real boot scene, client buffers and atomic scene latching.
//! React, CSS, window policy and product presentation remain outside this crate.

mod boot;
mod cursor;
mod input;
mod scanout;
mod session;

use std::{thread, time::Duration};

fn main() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("compositor: invariant failure: {info}")
    }));
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() == Some("--probe") && arguments.next().is_none() {
        std::process::exit(if scanout::Scanout::available() { 0 } else { 1 });
    }
    loop {
        match run() {
            Ok(()) => return,
            Err(error) => {
                eprintln!("compositor: {error}; retrying");
                thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = scanout::Scanout::open()?;
    let mut session = session::Session::open(scanout.device(), scanout.size())?;
    let size = scanout.size();
    let mut input = input::Input::open(size.width as i32, size.height as i32);
    let mut boot_offset = 0usize;
    let mut last_scene = None;
    loop {
        if let Some(scene) = session.poll(Duration::from_millis(33))? {
            scanout.compose(&scene, session.buffers(), input.position())?;
            let event = scanout.present(scene.revision)?;
            session.presented(&scene, event)?;
            last_scene = Some(scene);
        } else if !session.desktop_ready() {
            scanout.render_boot(boot_offset)?;
            scanout.present(0)?;
            boot_offset = (boot_offset + boot::SLIDER_STEP) % (boot::max_slider_offset() + 1);
        }
        if input.poll(&mut session)?
            && session.desktop_ready()
            && let Some(scene) = &last_scene
        {
            scanout.compose(scene, session.buffers(), input.position())?;
            scanout.present(input.cursor_revision())?;
        }
    }
}
