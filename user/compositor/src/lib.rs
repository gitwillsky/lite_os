//! LiteOS graphical compositor and display-session owner.
//!
//! The process owns DRM, scanout, the real boot scene, client buffers and atomic scene latching.
//! React, CSS, window policy and product presentation remain outside this crate.

mod boot;
mod clipboard;
mod cursor;
mod frame_stats;
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
    // Accumulates guest-vblank present intervals once the desktop reaches steady
    // state; owned here (not in Session) because the loop persists across epoch
    // resets while Session is torn down and rebuilt on desktop reconnect.
    let mut frame_stats = frame_stats::FrameStats::new();
    loop {
        // 1. Wait once on display sockets and evdev together. The wake borrow is
        //    scoped so `input` is free for mutable pumping below.
        let activity = {
            let wake = input.wake_fds();
            session.poll(&wake)?
        };
        // 2. A newly accepted scene is composed without the cursor, then the cursor
        //    is overlaid and the whole frame flipped.
        if activity.epoch_reset {
            // The desktop disconnected: the session dropped every client buffer
            // and the presented scene, but scanout still holds the last scene's
            // revisions, damage history and prepared damage. Return scanout to
            // boot so the next desktop's first compose is a full-screen paint,
            // not a stale diff over the previous session's pixels.
            scanout.reset_to_boot()?;
            // The next desktop restarts steady-state measurement from scratch;
            // drop any half-filled window from the dead epoch.
            frame_stats.reset();
        }
        if let Some(scene) = activity.scene {
            if !scene.is_discarded() {
                let event = if scene.output_size != scanout.size() {
                    match scanout.present_mode(&scene, session.buffers(), input.position())? {
                        scanout::ModePresent::Presented(event) => event,
                        scanout::ModePresent::Superseded(size) => {
                            session.discard_scene(&scene)?;
                            session.configure_output(size)?;
                            input.resize(size.width as i32, size.height as i32);
                            continue;
                        }
                    }
                } else {
                    scanout.compose(&scene, session.buffers(), session.scene_move(&scene))?;
                    scanout.present_scene(scene.revision, input.position())?
                };
                session.presented(&scene, event)?;
                // Measure only real desktop presents; `arm` excludes the static
                // fallback→desktop handoff interval from the baseline.
                if session.desktop_ready() {
                    frame_stats.arm();
                    frame_stats.record(frame_stats::flip_monotonic_ns(&event), event.sequence);
                }
            }
        }
        if let Some(size) = activity.output {
            input.resize(size.width as i32, size.height as i32);
        }
        // 3. Drain evdev whenever it signalled (also clears its readability so the
        //    next poll can block). A pure pointer move updates only the cursor via
        //    DIRTYFB, avoiding a scene recompose and page flip.
        if activity.input {
            let moved = input.poll(&mut session)?;
            if moved && session.desktop_ready() {
                let damage = session.take_move_damage();
                if let Some(active_move) = session.active_move()
                    && let Some(damage) = damage
                {
                    scanout.compose_move(
                        session.presented_nodes(),
                        session.buffers(),
                        active_move,
                        damage,
                        input.position(),
                    )?;
                } else {
                    scanout.move_cursor(input.position())?;
                }
            }
        }
        // 4. Apply the final cursor shape after routing this iteration's motion,
        //    so it reflects the surface now under the pointer: a pointer-focus
        //    change during `input.poll` resets the pending shape to the arrow,
        //    overriding a stale request from the surface just left. Only
        //    meaningful once the desktop drives the front buffer.
        if let Some(shape) = session.take_cursor_shape()
            && session.desktop_ready()
        {
            scanout.set_cursor_shape(shape, input.position())?;
        }
    }
}
