mod clients;
mod hotplug;

use display_client::{Device, Seat};

use crate::{
    diagnostics::Diagnostics,
    display::{Candidate, DamageRequest, Display, DisplayError},
    ffi::{self, PollFd},
    input::Input,
    presenter::Presenter,
    scene::Scene,
    server::Server,
};

const FRAME_INTERVAL_MS: u64 = 16;
const RESIZE_QUIET_MS: u64 = 50;

struct Active {
    display: Display,
    input: Input,
    drm: Device,
}

struct PreparedResize {
    mode: ffi::DrmMode,
    scene: Scene,
    buffers: Candidate,
}

enum ResizeFailure {
    Transient,
    Rejected(DisplayError),
}

pub fn run() -> Result<(), ()> {
    let mut presenter = core::pin::pin!(Presenter::new()?);
    presenter.as_mut().start()?;
    let result = run_session(presenter.as_ref().get_ref());
    presenter.as_mut().stop();
    result
}

fn run_session(presenter: &Presenter) -> Result<(), ()> {
    let netlink = hotplug::open()?;
    let mut server = match Server::open() {
        Ok(server) => server,
        Err(()) => {
            unsafe { ffi::close(netlink) };
            return Err(());
        }
    };
    let mut seat = match Seat::open() {
        Ok(seat) => seat,
        Err(()) => {
            unsafe { ffi::close(netlink) };
            return Err(());
        }
    };
    let (first, mut scene) = match Active::open(&mut seat, None) {
        Ok(active) => active,
        Err(()) => {
            unsafe { ffi::close(netlink) };
            return Err(());
        }
    };
    let mut active = Some(first);
    let mut diagnostics = Diagnostics::new();
    // OWNER: damage_pending 是 reactor 对 presenter 非 IDLE epoch 的唯一镜像；它在 submit
    // 后置位、completion 提交后清零。缺失该门禁会在 worker 仍读取 framebuffer 时 resize/RMFB。
    let mut damage_pending = false;
    let result = event_loop(
        &mut seat,
        netlink,
        &mut active,
        &mut scene,
        &mut server,
        presenter,
        &mut damage_pending,
        &mut diagnostics,
    );
    let release = active.take().map_or(Ok(()), |mut active| {
        drain_damage(
            presenter,
            &mut active,
            &mut damage_pending,
            &mut diagnostics,
        );
        active.release(&mut seat)
    });
    unsafe { ffi::close(netlink) };
    result.and(release)
}

fn event_loop(
    seat: &mut Seat,
    netlink: i32,
    active: &mut Option<Active>,
    scene: &mut Scene,
    server: &mut Server,
    presenter: &Presenter,
    damage_pending: &mut bool,
    diagnostics: &mut Diagnostics,
) -> Result<(), ()> {
    let mut render_due = None;
    let mut resize_due = None;
    let mut prepared_resize = None;
    let mut last_submit = ffi::monotonic_milliseconds();
    loop {
        let now = ffi::monotonic_milliseconds();
        let timeout = if active.is_some() {
            // completion eventfd 是在途 request 的唯一唤醒源。若仍保留 render/resize deadline，
            // blocking ioctl 期间会每 17 ms 空转 poll，重新制造输入掉帧时的 CPU 峰值。
            let (render_deadline, resize_deadline) = if *damage_pending {
                (None, None)
            } else {
                (render_due, resize_due)
            };
            deadline_timeout(render_deadline, resize_deadline, now)
        } else {
            -1
        };
        let (mut keyboard, mut pointer) = active.as_ref().map_or((-1, -1), |active| {
            (active.input.keyboard_fd(), active.input.pointer_fd())
        });
        if scene.terminal_focused() && !server.terminal_accepts_key_batch() {
            // The evdev OFD remains the backpressure owner until the terminal drains its
            // bounded event queue. Reading here would force key loss or an unbounded copy.
            keyboard = -1;
        }
        if scene.terminal_focused() && !server.terminal_accepts_pointer() {
            // Pointer events may carry terminal button state as well as cursor motion.
            // Leave the complete packet in evdev until the terminal event ring has space.
            pointer = -1;
        }
        let client_fds = server.client_fds();
        let client_events = server.client_events();
        let mut descriptors = [
            PollFd {
                fd: seat.fd(),
                events: ffi::POLLIN,
                returned: 0,
            },
            PollFd {
                fd: -1,
                events: ffi::POLLIN,
                returned: 0,
            },
            PollFd {
                fd: keyboard,
                events: ffi::POLLIN,
                returned: 0,
            },
            PollFd {
                fd: pointer,
                events: ffi::POLLIN,
                returned: 0,
            },
            PollFd {
                fd: presenter.completion_fd(),
                events: ffi::POLLIN,
                returned: 0,
            },
            PollFd {
                fd: netlink,
                events: ffi::POLLIN,
                returned: 0,
            },
            PollFd {
                fd: server.listener_fd(),
                events: ffi::POLLIN,
                returned: 0,
            },
            PollFd {
                fd: client_fds[0],
                events: client_events[0],
                returned: 0,
            },
            PollFd {
                fd: client_fds[1],
                events: client_events[1],
                returned: 0,
            },
            PollFd {
                fd: client_fds[2],
                events: client_events[2],
                returned: 0,
            },
        ];
        let ready = loop {
            let result = unsafe { ffi::poll(descriptors.as_mut_ptr(), descriptors.len(), timeout) };
            if result < 0 && ffi::errno() == ffi::EINTR {
                continue;
            }
            break result;
        };
        if ready < 0 || descriptors[0].returned & (ffi::POLLERR | ffi::POLLHUP) != 0 {
            return Err(());
        }
        if descriptors[6].returned & (ffi::POLLERR | ffi::POLLHUP) != 0 {
            return Err(());
        }
        let now = ffi::monotonic_milliseconds();
        if descriptors[0].returned & ffi::POLLIN != 0 {
            seat.dispatch()?;
            if let Some(enabled) = seat.take_change() {
                if enabled {
                    if active.is_some() {
                        return Err(());
                    }
                    let (next, next_scene) = Active::open(seat, Some(scene))?;
                    *scene = next_scene;
                    *active = Some(next);
                    last_submit = now;
                } else {
                    let mut current = active.take().ok_or(())?;
                    drain_damage(presenter, &mut current, damage_pending, diagnostics);
                    let release = current.release(seat);
                    let acknowledge = seat.acknowledge_disable();
                    release.and(acknowledge)?;
                    render_due = None;
                    resize_due = None;
                    prepared_resize.take();
                }
            }
        }
        let hotplug = if descriptors[5].returned & ffi::POLLIN != 0 {
            hotplug::drain(netlink)?
        } else {
            false
        };
        // Retire HUP/error slots before accepting peers from the same poll snapshot.
        // Otherwise an immediate same-identity restart is rejected as a duplicate even
        // though its predecessor has already closed the transport.
        let client_damage = clients::collect(server, scene, &descriptors[7..])?;
        if descriptors[6].returned & ffi::POLLIN != 0 {
            let (width, height) = active
                .as_ref()
                .map_or((0, 0), |current| current.display.dimensions());
            let snapshot = diagnostics.snapshot(
                width,
                height,
                *damage_pending,
                scene.preview_active(),
                server.client_mask(),
            );
            server.accept(&snapshot)?;
        }
        let Some(current) = active.as_mut() else {
            continue;
        };
        for descriptor in &descriptors[1..5] {
            if descriptor.returned & (ffi::POLLERR | ffi::POLLHUP) != 0 {
                return Err(());
            }
        }
        if !client_damage.rectangles().is_empty() {
            prepared_resize.take();
            for rectangle in client_damage.rectangles().iter().copied() {
                current.display.damage(rectangle);
            }
            schedule_render(&mut render_due, last_submit, now);
        }
        if descriptors[2].returned & ffi::POLLIN != 0 {
            let change = current.input.read_keyboard(scene)?;
            for key in change.keys() {
                if !server.queue_key(key.code, key.value)? {
                    return Err(());
                }
            }
            if change.quit {
                return Ok(());
            }
            if !change.damage.rectangles().is_empty() {
                prepared_resize.take();
                for rectangle in change.damage.rectangles().iter().copied() {
                    current.display.damage(rectangle);
                }
                schedule_render(&mut render_due, last_submit, now);
            }
        }
        if descriptors[3].returned & ffi::POLLIN != 0 {
            let change = current.input.read_pointer(scene)?;
            if let Some(since_ms) = change.damage_since_ms {
                diagnostics.pointer_input(since_ms);
            }
            if let Some(event) = change.event {
                let _ = server.queue_click(event)?;
            }
            if let Some(event) = change.pointer
                && !server.queue_terminal_pointer(event)?
            {
                return Err(());
            }
            if !change.damage.rectangles().is_empty() {
                prepared_resize.take();
            }
            for rectangle in change.damage.rectangles().iter().copied() {
                current.display.damage(rectangle);
            }
            if !change.damage.rectangles().is_empty() {
                // Pointer/click is the software-cursor fast lane. The single presenter slot
                // still serializes DIRTYFB; waiting for the normal scene cadence here adds a
                // full refresh interval before a tiny overlay can become visible.
                render_due = Some(now);
            }
        }
        if descriptors[4].returned & ffi::POLLIN != 0 {
            finish_damage(presenter, current, damage_pending, diagnostics, now, false)?.ok_or(())?;
            if current.display.has_damage() {
                schedule_render(&mut render_due, last_submit, now);
            } else {
                render_due = None;
            }
        }
        if hotplug {
            diagnostics.resize_notice();
            prepared_resize.take();
            resize_due = Some(now.saturating_add(RESIZE_QUIET_MS));
        }
        if let Some(configuration) = scene.grid_configuration() {
            let _ = server.queue_grid_configuration(configuration)?;
        }
        if !*damage_pending && resize_due.is_some_and(|deadline| deadline <= now) {
            diagnostics.resize_attempt();
            match resize(current, scene, &mut prepared_resize) {
                Ok(changed) => {
                    resize_due = None;
                    if changed {
                        diagnostics.resize_commit(now);
                        render_due = None;
                        last_submit = now;
                    }
                }
                Err(ResizeFailure::Transient) => {
                    diagnostics.resize_transient();
                    resize_due = Some(now.saturating_add(FRAME_INTERVAL_MS));
                }
                Err(ResizeFailure::Rejected(error)) => {
                    diagnostics.resize_rejected();
                    prepared_resize.take();
                    resize_due = None;
                    report_resize_failure(error);
                }
            }
        }
        if !*damage_pending && render_due.is_some_and(|deadline| deadline <= now) {
            match current.display.prepare_damage(scene)? {
                Some(request) => {
                    let metrics = request.metrics();
                    submit_damage(presenter, &mut current.display, request)?;
                    last_submit = now;
                    *damage_pending = true;
                    diagnostics.frame_submitted(metrics)?;
                    render_due = None;
                }
                None if current.display.has_damage() => {
                    render_due = Some(now.saturating_add(FRAME_INTERVAL_MS));
                }
                None => render_due = None,
            }
        }
    }
}

/// @description 发布唯一 presenter request；publication failure 会先归还 Display snapshot。
/// @param presenter 固定 SPSC worker owner。
/// @param display request 对应 framebuffer/inflight owner。
/// @param request prepare_damage 返回的自包含 request。
/// @return request 成功交给 worker 时返回 unit。
/// @errors presenter protocol/publication failure 返回 unit error，damage 已恢复可清理状态。
fn submit_damage(
    presenter: &Presenter,
    display: &mut Display,
    request: DamageRequest,
) -> Result<(), ()> {
    match presenter.submit(request) {
        Ok(true) => Ok(()),
        Ok(false) | Err(()) => {
            let _ = display.complete_damage(request, ffi::EBUSY);
            Err(())
        }
    }
}

/// @description 收割一个 presenter completion，并原子提交或恢复 Display damage snapshot。
/// @param presenter 固定 SPSC worker owner。
/// @param active 当前仍拥有 request framebuffer 的 display session。
/// @param pending reactor 对唯一在途 request 的事实；completion 后清零。
/// @param wait revoke/exit cleanup 为 true，普通 poll path 为 false。
/// @return 无 request 为 None；完成时返回 DIRTYFB 是否成功。
/// @errors presenter protocol、request owner 或不可恢复 DIRTYFB failure 返回 unit error。
fn finish_damage(
    presenter: &Presenter,
    active: &mut Active,
    pending: &mut bool,
    diagnostics: &mut Diagnostics,
    now_ms: u64,
    wait: bool,
) -> Result<Option<bool>, ()> {
    if !*pending {
        return Ok(None);
    }
    let (request, error) = presenter.completion(wait)?.ok_or(())?;
    *pending = false;
    let completed = active.display.complete_damage(request, error)?;
    diagnostics.frame_completed(now_ms, completed);
    Ok(Some(completed))
}

/// @description 在 framebuffer cleanup 前同步收回 presenter ownership；协议损坏时 fail-stop。
/// @param presenter 固定 SPSC worker owner。
/// @param active 仍拥有 request framebuffer 的 display session。
/// @param pending reactor 对唯一在途 request 的事实。
fn drain_damage(
    presenter: &Presenter,
    active: &mut Active,
    pending: &mut bool,
    diagnostics: &mut Diagnostics,
) {
    if finish_damage(
        presenter,
        active,
        pending,
        diagnostics,
        ffi::monotonic_milliseconds(),
        true,
    )
    .is_err()
    {
        // 无法证明 worker 已归还 framebuffer 时，任何 Rust unwind/drop 都可能触发并发 RMFB。
        // _exit 终止整个进程及 worker，由内核按 file lifetime 回收 DRM 对象。
        unsafe { ffi::_exit(126) };
    }
}

fn resize(
    active: &mut Active,
    scene: &mut Scene,
    prepared: &mut Option<PreparedResize>,
) -> Result<bool, ResizeFailure> {
    let mode = active.display.query_mode().map_err(resize_error)?;
    if !active.display.mode_changed(mode) {
        prepared.take();
        return Ok(false);
    }
    if prepared
        .as_ref()
        .is_some_and(|candidate| !same_mode(candidate.mode, mode))
    {
        prepared.take();
    }
    if prepared.is_none() {
        let candidate_scene = scene
            .try_resized(usize::from(mode.hdisplay), usize::from(mode.vdisplay))
            .map_err(|_| ResizeFailure::Rejected(DisplayError::OutOfMemory))?;
        let buffers = active
            .display
            .prepare(mode, &candidate_scene)
            .map_err(resize_error)?;
        *prepared = Some(PreparedResize {
            mode,
            scene: candidate_scene,
            buffers,
        });
        let confirmed = active.display.query_mode().map_err(resize_error)?;
        if !same_mode(mode, confirmed) {
            prepared.take();
            return Err(ResizeFailure::Transient);
        }
    }
    let result = active
        .display
        .commit(&mut prepared.as_mut().ok_or(ResizeFailure::Transient)?.buffers);
    if let Err(error) = result {
        return Err(resize_error(error));
    }
    *scene = prepared.take().ok_or(ResizeFailure::Transient)?.scene;
    Ok(true)
}

fn resize_error(error: DisplayError) -> ResizeFailure {
    if error == DisplayError::Transient {
        ResizeFailure::Transient
    } else {
        ResizeFailure::Rejected(error)
    }
}

fn same_mode(first: ffi::DrmMode, second: ffi::DrmMode) -> bool {
    first.hdisplay == second.hdisplay && first.vdisplay == second.vdisplay
}

fn report_resize_failure(error: DisplayError) {
    let message: &[u8] = match error {
        DisplayError::Transient => return,
        DisplayError::OutOfMemory => {
            b"liteui-compositor: resize out of memory; preserving active mode\n"
        }
        DisplayError::System => {
            b"liteui-compositor: resize transaction rejected; preserving active mode\n"
        }
    };
    let mut written = 0;
    while written < message.len() {
        let count = unsafe {
            ffi::write(
                2,
                message[written..].as_ptr().cast(),
                message.len() - written,
            )
        };
        if count > 0 {
            written += count as usize;
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else {
            break;
        }
    }
}

impl Active {
    fn open(seat: &mut Seat, previous: Option<&Scene>) -> Result<(Self, Scene), ()> {
        let drm = seat.open_device(ffi::c_str(b"/dev/dri/card0\0"))?;
        let mut display = match Display::open(drm.fd) {
            Ok(display) => display,
            Err(()) => {
                seat.close_device(drm)?;
                return Err(());
            }
        };
        let (width, height) = display.dimensions();
        let scene = previous.map_or_else(
            || Scene::try_new(width, height),
            |scene| scene.try_resized(width, height),
        )?;
        if display.activate(&scene).is_err() {
            drop(display);
            seat.close_device(drm)?;
            return Err(());
        }
        let input = match Input::open(seat) {
            Ok(input) => input,
            Err(()) => {
                drop(display);
                seat.close_device(drm)?;
                return Err(());
            }
        };
        Ok((
            Self {
                display,
                input,
                drm,
            },
            scene,
        ))
    }

    fn release(self, seat: &mut Seat) -> Result<(), ()> {
        let Self {
            display,
            input,
            drm,
        } = self;
        drop(display);
        let input_result = input.release(seat);
        let drm_result = seat.close_device(drm);
        input_result.and(drm_result)
    }
}

fn schedule_render(deadline: &mut Option<u64>, last_present: u64, now: u64) {
    if deadline.is_none() {
        *deadline = Some(if now.saturating_sub(last_present) >= FRAME_INTERVAL_MS {
            now
        } else {
            last_present.saturating_add(FRAME_INTERVAL_MS)
        });
    }
}

fn deadline_timeout(render: Option<u64>, resize: Option<u64>, now: u64) -> i32 {
    [render, resize]
        .into_iter()
        .flatten()
        .min()
        .map_or(-1, |deadline| {
            i32::try_from(deadline.saturating_sub(now)).unwrap_or(i32::MAX)
        })
}
