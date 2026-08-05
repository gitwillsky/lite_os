//! LiteOS graphical compositor and display-session owner.
//!
//! The process owns DRM, scanout, the real boot scene, client buffers and atomic scene latching.
//! React, CSS, window policy and product presentation remain outside this crate.

mod boot;
mod cursor;
mod frame_stats;
mod gpu;
mod input;
mod scanout;
mod session;
mod spice_agent;

use std::{io, thread, time::Duration};

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
    let mut session = session::Session::open(scanout.device(), scanout.graphics(), scanout.size())?;
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
        // Only compositor-owned window dragging uses asynchronous page flips now;
        // ordinary pointer motion runs on the independent hardware cursor queue.
        let mut redraw_move = if activity.flip {
            finish_move_frame(&mut scanout, &session, &mut frame_stats)?
        } else {
            false
        };
        // Client React/CSS produces only immutable paint commands. The
        // compositor resolves its private textures and performs the complete
        // raster in the sole VirGL context before any scene can adopt it.
        for owner in activity.paint.iter().copied() {
            let size = session.paint_size(owner)?;
            let target = match session.take_paint_target(owner, size) {
                Some(target) => target,
                None => session::PaintTarget {
                    pixels: scanout.create_paint_target(size)?,
                    repair: Some(display_proto::Rect {
                        x: 0,
                        y: 0,
                        width: size.width,
                        height: size.height,
                    }),
                },
            };
            let list = session.paint_list(owner)?;
            let revision = list.revision;
            let configuration_serial = list.configuration_serial;
            let damage = list.damage;
            let cache_owner = session.paint_cache_owner(owner);
            let base = if list.base_revision == 0 {
                None
            } else {
                Some(
                    session
                        .paint_base(owner, list.base_revision)
                        .ok_or_else(|| {
                            io::Error::other("retained paint base revision disappeared")
                        })?,
                )
            };
            let repair = target.repair;
            let target = scanout.render_display_list(
                target.pixels,
                list,
                base,
                repair,
                cache_owner,
                |texture_id| session.paint_texture(owner, texture_id),
            )?;
            session.publish_paint(
                owner,
                target,
                revision,
                configuration_serial,
                damage,
            )?;
        }
        // A move underlay is needed only after the desktop authorizes an exact
        // pointer-down. Eagerly rendering one full-output texture per window on
        // every scene made ordinary hover and typing pay seconds of unrelated
        // GPU work.
        if let Some(request) = activity.move_begin {
            let desktop = scanout.render_display_list_excluding(
                session.paint_size(session::Owner::Desktop)?,
                session.paint_list(session::Owner::Desktop)?,
                |texture_id| session.paint_texture(session::Owner::Desktop, texture_id),
                request.surface_id,
            )?;
            let target = scanout.compose_move_underlay(
                desktop,
                session.presented_nodes(),
                session.buffers(),
                request.surface_id,
            )?;
            if let Some(error) = session.begin_move(request, target)? {
                eprintln!("compositor: move grab rejected: {error}");
            }
        }
        // 2. A newly accepted scene is composed and flipped independently from the
        //    hardware cursor plane.
        if activity.epoch_reset {
            if scanout.move_frame_in_flight() {
                let _ = finish_move_frame(&mut scanout, &session, &mut frame_stats)?;
            }
            // The desktop disconnected: the session dropped every client buffer
            // and the presented scene, but both GPU scanout targets still hold
            // the last epoch's pixels and revisions. Return scanout to boot so
            // the next desktop cannot inherit stale content.
            scanout.reset_to_boot()?;
            // The next desktop restarts steady-state measurement from scratch;
            // drop any half-filled window from the dead epoch.
            frame_stats.reset();
            redraw_move = false;
        }
        if let Some(scene) = activity.scene
            && !scene.is_discarded()
        {
            if scanout.move_frame_in_flight() {
                let _ = finish_move_frame(&mut scanout, &session, &mut frame_stats)?;
            }
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
                scanout.present_scene(scene.revision, scene.damage, input.position())?
            };
            session.presented(&scene, event)?;
            // The scene frame already sampled the move transform, so an older
            // coalesced drag redraw is obsolete. Cursor state is independent.
            redraw_move = false;
            // Measure only real desktop presents; `arm` excludes the static
            // fallback→desktop handoff interval from the baseline.
            if session.desktop_ready() {
                frame_stats.arm();
                frame_stats.record(frame_stats::flip_monotonic_ns(&event), event.sequence);
            }
        }
        if let Some(size) = activity.output {
            input.resize(size.width as i32, size.height as i32);
        }
        if redraw_move
            && session.desktop_ready()
            && let Some(active_move) = session.active_move()
        {
            scanout.compose_latest_move(
                session.presented_nodes(),
                session.buffers(),
                active_move,
            )?;
        }
        // 3. Drain evdev whenever it signalled. Every accepted motion updates the
        //    cursor fast path directly; only an active window drag also redraws scene pixels.
        if activity.input {
            let cursor_ready = session.desktop_ready();
            let moved = input.poll(&mut session, |position| {
                if cursor_ready {
                    scanout.move_cursor(position)
                } else {
                    Ok(())
                }
            })?;
            if moved && session.desktop_ready() {
                if session.take_move_changed()
                    && let Some(active_move) = session.active_move()
                {
                    scanout.compose_move(
                        session.presented_nodes(),
                        session.buffers(),
                        active_move,
                    )?;
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

/// Retires one compositor-owned move flip through the same timing owner as a scene flip.
fn finish_move_frame(
    scanout: &mut scanout::Scanout,
    session: &session::Session,
    frame_stats: &mut frame_stats::FrameStats,
) -> io::Result<bool> {
    let (redraw, event) = scanout.finish_move_frame()?;
    if session.desktop_ready() {
        frame_stats.arm();
        frame_stats.record(frame_stats::flip_monotonic_ns(&event), event.sequence);
    }
    Ok(redraw)
}
