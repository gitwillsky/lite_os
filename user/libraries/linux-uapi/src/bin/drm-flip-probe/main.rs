//! Kernel display/GPU probe: VirGL execution, readback and page-flip cadence without any UI.
//!
//! The probe owns `/dev/dri/card0` alone (the host gate boots a private image that does not start
//! the compositor, so DRM master is free) and exercises the same kernel path the product uses:
//! VirGL context -> render target -> `EXECBUFFER` clear -> `PAGE_FLIP` -> vblank event. It reports
//! one result line for `scripts/verify_gpu.py`; timestamps come from the kernel's monotonic event
//! clock, so host scheduling jitter does not enter the measurement.

mod fdinfo;
mod sharing;

use std::{io, process::ExitCode};

use linux_uapi::drm::{CommandEncoder, DrmDevice, ObjectKind, VirglContext, VirglResource};

/// Frames submitted before measurement starts; they absorb scanout setup and host shader warmup.
const WARMUP_FRAMES: u64 = 60;
/// Frames whose completion intervals are measured.
const MEASURED_FRAMES: u64 = 600;
/// `VirtIO-GPU` surface object id inside the probe's private VirGL context.
const TARGET_SURFACE: u32 = 8;
/// Edge of the readback rectangle.
const READBACK_EDGE: u32 = 64;
/// Per-channel tolerance for the readback comparison (host float-to-unorm rounding).
const READBACK_TOLERANCE: u8 = 2;
const READBACK_COLOR: [f32; 4] = [0.25, 0.5, 0.75, 1.0];

/// Completion intervals and sequence gaps of the measured window.
#[derive(Debug, PartialEq, Eq)]
struct FlipStats {
    frames: usize,
    dropped: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
}

/// Summarizes consecutive flip completions.
///
/// # Parameters
///
/// - `events`: `(kernel monotonic microseconds, device presentation sequence)` in completion order.
///
/// # Returns
///
/// `None` with fewer than two events (no interval exists). Otherwise nearest-rank percentiles of
/// the intervals, and the count of sequence numbers skipped between adjacent events.
fn summarize(events: &[(u64, u32)]) -> Option<FlipStats> {
    if events.len() < 2 {
        return None;
    }
    let mut intervals: Vec<u64> = events
        .windows(2)
        .map(|pair| pair[1].0.saturating_sub(pair[0].0))
        .collect();
    let dropped = events
        .windows(2)
        .map(|pair| u64::from(pair[1].1.wrapping_sub(pair[0].1).saturating_sub(1)))
        .sum();
    intervals.sort_unstable();
    let percentile = |percent: usize| {
        let rank = (intervals.len() * percent).div_ceil(100).max(1);
        intervals[rank - 1]
    };
    Some(FlipStats {
        frames: events.len(),
        dropped,
        p50_us: percentile(50),
        p95_us: percentile(95),
        p99_us: percentile(99),
    })
}

/// Unpacks a `B8G8R8X8` pixel into `[r, g, b]`.
fn rgb(pixel: u32) -> [u8; 3] {
    [(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]
}

/// Returns whether `pixel` is `color` within the rounding tolerance.
fn matches_color(pixel: u32, color: [f32; 4]) -> bool {
    rgb(pixel).into_iter().zip(color).all(|(actual, expected)| {
        actual.abs_diff((expected * 255.0).round() as u8) <= READBACK_TOLERANCE
    })
}

/// One render target registered as a scanout framebuffer.
struct Target {
    resource: VirglResource,
    framebuffer_id: u32,
}

fn step<T>(name: &str, result: io::Result<T>) -> Result<T, String> {
    result.map_err(|error| {
        let error = format!("{name}: {error}");
        eprintln!("drm-flip-probe: error: {error}");
        error
    })
}

fn clear_commands(target: &VirglResource, color: [f32; 4]) -> CommandEncoder {
    let mut commands = CommandEncoder::new();
    commands.create_surface(TARGET_SURFACE, target.resource_id(), target.format());
    commands.set_framebuffer(TARGET_SURFACE);
    commands.set_viewport(target.width(), target.height());
    commands.clear(color);
    commands.destroy_object(ObjectKind::Surface, TARGET_SURFACE);
    commands
}

/// Clears a render target on the host GPU and verifies the pixels by reading them back.
fn verify_readback(graphics: &VirglContext, target: &mut Target) -> Result<(), String> {
    let commands = clear_commands(&target.resource, READBACK_COLOR);
    step(
        "readback exec",
        graphics.exec(commands.words(), &[&target.resource]),
    )?;
    step("readback exec wait", target.resource.wait())?;
    step(
        "readback transfer",
        target
            .resource
            .transfer_from_host(0, 0, READBACK_EDGE, READBACK_EDGE),
    )?;
    step("readback transfer wait", target.resource.wait())?;
    let pixel = target.resource.row(0)[0];
    if matches_color(pixel, READBACK_COLOR) {
        Ok(())
    } else {
        Err(format!(
            "readback: pixel {pixel:#010x} is not the cleared color {READBACK_COLOR:?}"
        ))
    }
}

fn run() -> Result<String, String> {
    step("fdinfo", fdinfo::verify())?;
    // fdinfo 仅依赖 dumb-buffer，用独立阶段在无 VirGL 的设备上验证统计，不跳过完整 GPU 门禁。
    if std::env::args().nth(1).as_deref() == Some("--fdinfo-only") {
        return Ok("drm-flip-probe: fdinfo phase complete".into());
    }
    sharing::verify()?;
    let device = step("open /dev/dri/card0", DrmDevice::open("/dev/dri/card0"))?;
    let topology = step("query topology", device.query_topology())?;
    let (width, height) = (
        u32::from(topology.mode.width()),
        u32::from(topology.mode.height()),
    );
    let graphics = step(
        "initialize VirGL",
        device.initialize_virgl("liteos-drm-flip-probe"),
    )?;
    step("set master", device.set_master())?;
    let mut targets = Vec::new();
    for _ in 0..2 {
        let resource = step(
            "create render target",
            graphics.create_render_target(width, height),
        )?;
        let framebuffer_id = step(
            "add framebuffer",
            device.add_virgl_framebuffer(&resource, 24),
        )?;
        targets.push(Target {
            resource,
            framebuffer_id,
        });
    }
    verify_readback(&graphics, &mut targets[0])?;
    step(
        "set crtc",
        device.set_crtc(&topology, targets[0].framebuffer_id),
    )?;

    let mut front = 0;
    let mut completions = Vec::with_capacity(MEASURED_FRAMES as usize);
    for frame in 0..WARMUP_FRAMES + MEASURED_FRAMES {
        let back = 1 - front;
        let shade = (frame % 256) as f32 / 255.0;
        let commands = clear_commands(&targets[back].resource, [shade, 1.0 - shade, 0.5, 1.0]);
        step(
            "frame exec",
            graphics.exec(commands.words(), &[&targets[back].resource]),
        )?;
        step(
            "page flip",
            device.page_flip(&topology, targets[back].framebuffer_id, frame),
        )?;
        let event = step("flip event", device.read_flip_event())?;
        if event.user_data != frame {
            return Err(format!(
                "flip event: completed frame {} while frame {frame} was in flight",
                event.user_data
            ));
        }
        if frame >= WARMUP_FRAMES {
            let microseconds = u64::from(event.seconds) * 1_000_000 + u64::from(event.microseconds);
            completions.push((microseconds, event.sequence));
        }
        front = back;
    }
    let stats = summarize(&completions).ok_or("no flip interval was measured")?;
    Ok(format!(
        "drm-flip-probe: mode={width}x{height} readback=ok frames={} dropped={} p50_us={} p95_us={} p99_us={}",
        stats.frames, stats.dropped, stats.p50_us, stats.p95_us, stats.p99_us
    ))
}

fn main() -> ExitCode {
    let code = match run() {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            println!("drm-flip-probe: error: {error}");
            ExitCode::FAILURE
        }
    };
    // The gate stops reading at this line; printing it after the result guarantees the result
    // line was not split across two serial reads.
    println!("drm-flip-probe: done");
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_uses_nearest_rank_percentiles_over_adjacent_intervals() {
        // Intervals: 16, 17, 16, 40, 16 -> sorted 16, 16, 16, 17, 40.
        let events = [(0, 1), (16, 2), (33, 3), (49, 4), (89, 5), (105, 6)];
        assert_eq!(
            summarize(&events),
            Some(FlipStats {
                frames: 6,
                dropped: 0,
                p50_us: 16,
                p95_us: 40,
                p99_us: 40,
            })
        );
    }

    #[test]
    fn skipped_sequence_numbers_count_as_dropped_frames() {
        let events = [(0, 10), (16, 11), (50, 14), (66, 15)];
        assert_eq!(summarize(&events).unwrap().dropped, 2);
    }

    #[test]
    fn sequence_wraparound_is_not_a_drop() {
        let events = [(0, u32::MAX), (16, 0), (32, 1)];
        assert_eq!(summarize(&events).unwrap().dropped, 0);
    }

    #[test]
    fn fewer_than_two_events_have_no_interval() {
        assert_eq!(summarize(&[]), None);
        assert_eq!(summarize(&[(1, 1)]), None);
    }

    #[test]
    fn readback_tolerates_rounding_but_not_a_wrong_channel() {
        let exact = (64 << 16) | (128 << 8) | 191;
        assert!(matches_color(exact, READBACK_COLOR));
        assert!(matches_color(exact + 2, READBACK_COLOR));
        assert!(!matches_color(exact + 3, READBACK_COLOR));
        assert!(!matches_color(0, READBACK_COLOR));
    }
}
