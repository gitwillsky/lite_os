use display_client::{Device, Seat};

use crate::{
    display::{Candidate, DamageRequest, DamageTarget, Display, DisplayError},
    ffi::{self, PollFd, SockaddrNl},
    input::Input,
    presenter::Presenter,
    scene::Scene,
};

const FRAME_INTERVAL_MS: u64 = 17;
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
    let netlink = open_hotplug()?;
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
    // OWNER: damage_pending 是 reactor 对 presenter 非 IDLE epoch 的唯一镜像；它在 submit
    // 后置位、completion 提交后清零。缺失该门禁会在 worker 仍读取 framebuffer 时 resize/RMFB。
    let mut damage_pending = false;
    let result = event_loop(
        &mut seat,
        netlink,
        &mut active,
        &mut scene,
        presenter,
        &mut damage_pending,
    );
    let release = active.take().map_or(Ok(()), |mut active| {
        drain_damage(presenter, &mut active, &mut damage_pending);
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
    presenter: &Presenter,
    damage_pending: &mut bool,
) -> Result<(), ()> {
    let mut render_due = None;
    let mut resize_due = None;
    let mut prepared_resize = None;
    // geometry 必须切换到已同步的备用 buffer；若缺少此标记，pointer damage 也会退化为整帧 page flip。
    let mut flip_requested = false;
    let mut last_present = ffi::monotonic_milliseconds();
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
        let (drm, keyboard, pointer) = active.as_ref().map_or((-1, -1, -1), |active| {
            (
                active.drm.fd,
                active.input.keyboard_fd(),
                active.input.pointer_fd(),
            )
        });
        let mut descriptors = [
            PollFd {
                fd: seat.fd(),
                events: ffi::POLLIN,
                returned: 0,
            },
            PollFd {
                fd: drm,
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
        let now = ffi::monotonic_milliseconds();
        if descriptors[0].returned & ffi::POLLIN != 0 {
            seat.dispatch()?;
            if let Some(enabled) = seat.take_change() {
                if enabled {
                    if active.is_some() {
                        return Err(());
                    }
                    let (next, next_scene) = Active::open(seat, Some(*scene))?;
                    *scene = next_scene;
                    *active = Some(next);
                    flip_requested = false;
                    last_present = now;
                } else {
                    let mut current = active.take().ok_or(())?;
                    drain_damage(presenter, &mut current, damage_pending);
                    let release = current.release(seat);
                    let acknowledge = seat.acknowledge_disable();
                    release.and(acknowledge)?;
                    render_due = None;
                    resize_due = None;
                    prepared_resize.take();
                    flip_requested = false;
                }
            }
        }
        let hotplug = if descriptors[5].returned & ffi::POLLIN != 0 {
            drain_hotplug(netlink)?
        } else {
            false
        };
        let Some(current) = active.as_mut() else {
            continue;
        };
        for descriptor in &descriptors[1..5] {
            if descriptor.returned & (ffi::POLLERR | ffi::POLLHUP) != 0 {
                return Err(());
            }
        }
        if descriptors[1].returned & ffi::POLLIN != 0 {
            current.display.read_events()?;
        }
        if descriptors[2].returned & ffi::POLLIN != 0 {
            let change = current.input.read_keyboard(scene)?;
            if change.quit {
                return Ok(());
            }
            if let Some(damage) = change.damage {
                prepared_resize.take();
                current.display.damage(damage);
                flip_requested = true;
                schedule_render(&mut render_due, last_present, now);
            }
        }
        if descriptors[3].returned & ffi::POLLIN != 0
            && let Some(damage) = current.input.read_pointer(scene)?
        {
            prepared_resize.take();
            for rectangle in damage {
                current.display.damage(rectangle);
            }
            schedule_render(&mut render_due, last_present, now);
        }
        if descriptors[4].returned & ffi::POLLIN != 0 {
            let (completed, prepares_flip) =
                finish_damage(presenter, current, damage_pending, false)?.ok_or(())?;
            if completed && !prepares_flip {
                last_present = now;
            }
            if flip_requested || current.display.has_active_damage() {
                schedule_render(&mut render_due, last_present, now);
            } else {
                render_due = None;
            }
        }
        if hotplug {
            prepared_resize.take();
            resize_due = Some(now.saturating_add(RESIZE_QUIET_MS));
        }
        if !*damage_pending && resize_due.is_some_and(|deadline| deadline <= now) {
            match resize(current, scene, &mut prepared_resize) {
                Ok(changed) => {
                    resize_due = None;
                    if changed {
                        render_due = None;
                        flip_requested = false;
                        last_present = now;
                    }
                }
                Err(ResizeFailure::Transient) => {
                    resize_due = Some(now.saturating_add(FRAME_INTERVAL_MS));
                }
                Err(ResizeFailure::Rejected(error)) => {
                    prepared_resize.take();
                    resize_due = None;
                    report_resize_failure(error);
                }
            }
        }
        if !*damage_pending && render_due.is_some_and(|deadline| deadline <= now) {
            if flip_requested && current.display.present_flip()? {
                last_present = now;
                render_due = None;
                flip_requested = false;
            } else {
                let target = if flip_requested {
                    DamageTarget::Flip
                } else {
                    DamageTarget::Active
                };
                match current.display.prepare_damage(scene, target)? {
                    Some(request) => {
                        submit_damage(presenter, &mut current.display, request)?;
                        *damage_pending = true;
                        render_due = None;
                    }
                    None if if flip_requested {
                        current.display.has_flip_work()
                    } else {
                        current.display.has_active_damage()
                    } => {
                        render_due = Some(now.saturating_add(FRAME_INTERVAL_MS));
                    }
                    None => render_due = None,
                }
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
/// @return 无 request 为 None；完成时返回（成功、是否准备 flip）。
/// @errors presenter protocol、request owner 或不可恢复 DIRTYFB failure 返回 unit error。
fn finish_damage(
    presenter: &Presenter,
    active: &mut Active,
    pending: &mut bool,
    wait: bool,
) -> Result<Option<(bool, bool)>, ()> {
    if !*pending {
        return Ok(None);
    }
    let (request, error) = presenter.completion(wait)?.ok_or(())?;
    *pending = false;
    let prepares_flip = request.prepares_flip();
    let completed = active.display.complete_damage(request, error)?;
    Ok(Some((completed, prepares_flip)))
}

/// @description 在 framebuffer cleanup 前同步收回 presenter ownership；协议损坏时 fail-stop。
/// @param presenter 固定 SPSC worker owner。
/// @param active 仍拥有 request framebuffer 的 display session。
/// @param pending reactor 对唯一在途 request 的事实。
fn drain_damage(presenter: &Presenter, active: &mut Active, pending: &mut bool) {
    if finish_damage(presenter, active, pending, true).is_err() {
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
    if active.display.flip_pending() {
        return Err(ResizeFailure::Transient);
    }
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
        let candidate_scene = scene.resized(usize::from(mode.hdisplay), usize::from(mode.vdisplay));
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
        DisplayError::OutOfMemory => b"liteos-2d: resize out of memory; preserving active mode\n",
        DisplayError::System => b"liteos-2d: resize transaction rejected; preserving active mode\n",
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
    fn open(seat: &mut Seat, previous: Option<Scene>) -> Result<(Self, Scene), ()> {
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
            || Scene::new(width, height),
            |scene| scene.resized(width, height),
        );
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

fn open_hotplug() -> Result<i32, ()> {
    let fd = unsafe {
        ffi::socket(
            i32::from(ffi::AF_NETLINK),
            ffi::SOCK_DGRAM | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
            ffi::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(());
    }
    let address = SockaddrNl {
        family: ffi::AF_NETLINK,
        padding: 0,
        port_id: 0,
        groups: 1,
    };
    if unsafe { ffi::bind(fd, &address, core::mem::size_of::<SockaddrNl>() as u32) } < 0 {
        unsafe { ffi::close(fd) };
        return Err(());
    }
    Ok(fd)
}

fn drain_hotplug(fd: i32) -> Result<bool, ()> {
    let mut received = false;
    let mut bytes = [0u8; 512];
    loop {
        let count = unsafe { ffi::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count > 0 {
            received = true;
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else if count < 0 && ffi::errno() == ffi::EAGAIN {
            return Ok(received);
        } else {
            return Err(());
        }
    }
}
