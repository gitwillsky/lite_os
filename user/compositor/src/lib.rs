//! LiteOS graphical compositor and display-session owner.
//!
//! The process owns DRM, scanout, the real boot scene, client buffers and atomic scene latching.
//! React, CSS, window policy and product presentation remain outside this crate.

mod boot;
mod cursor;
mod frame_stats;
mod input;
mod scanout;
mod session;

use std::{
    thread,
    time::{Duration, Instant},
};

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

/// Compositor poll cadence.
///
/// It bounds the boot animation to ~30 Hz and caps how long the loop parks when
/// idle; input and scene readiness wake it earlier through the shared `poll`.
const FRAME: Duration = Duration::from_millis(33);

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = scanout::Scanout::open()?;
    let mut session = session::Session::open(scanout.device(), scanout.size())?;
    let size = scanout.size();
    let mut input = input::Input::open(size.width as i32, size.height as i32);
    let mut boot_offset = 0usize;
    // Accumulates guest-vblank present intervals once the desktop reaches steady
    // state; owned here (not in Session) because the loop persists across epoch
    // resets while Session is torn down and rebuilt on desktop reconnect.
    let mut frame_stats = frame_stats::FrameStats::new();
    // Throttles the boot slider to FRAME regardless of what woke the loop.
    //
    // Input fds now share the poll wait, so pointer motion can return early and
    // repeatedly; gating on elapsed time keeps the pre-desktop animation at a
    // steady 30 Hz instead of racing ahead on every stray event.
    let mut last_boot = Instant::now() - FRAME;
    loop {
        // 1. Wait once on display sockets and evdev together. The wake borrow is
        //    scoped so `input` is free for mutable pumping below.
        let activity = {
            let wake = input.wake_fds();
            session.poll(&wake, FRAME)?
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
            boot_offset = 0;
            last_boot = Instant::now() - FRAME;
            // The next desktop restarts steady-state measurement from scratch;
            // drop any half-filled window from the dead epoch.
            frame_stats.reset();
        }
        if let Some(scene) = activity.scene {
            scanout.compose(&scene, session.buffers(), session.scene_move(&scene))?;
            let event = scanout.present_scene(scene.revision, input.position())?;
            session.presented(&scene, event)?;
            // Measure only real desktop presents; boot-animation frames precede
            // `desktop_ready` and `arm` clears the baseline so the boot→desktop
            // transition interval is excluded.
            if session.desktop_ready() {
                frame_stats.arm();
                frame_stats.record(frame_stats::flip_monotonic_ns(&event), event.sequence);
            }
        } else if !session.desktop_ready() && last_boot.elapsed() >= FRAME {
            scanout.render_boot(boot_offset)?;
            scanout.present(0)?;
            boot_offset = (boot_offset + boot::SLIDER_STEP) % (boot::max_slider_offset() + 1);
            last_boot = Instant::now();
        }
        // A client requested a new pointer cursor shape: switch the asset and
        // repaint the cursor in place. Only meaningful once the desktop is up
        // and driving the front buffer; before that the boot scene owns scanout.
        if let Some(shape) = activity.cursor_shape
            && session.desktop_ready()
        {
            scanout.set_cursor_shape(shape, input.position())?;
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
    }
}
