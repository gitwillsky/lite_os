use display_client::{Device, Seat};

use crate::{
    display::{Candidate, Display, DisplayError},
    ffi::{self, PollFd, SockaddrNl},
    input::Input,
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
    let result = event_loop(&mut seat, netlink, &mut active, &mut scene);
    let release = active
        .take()
        .map_or(Ok(()), |active| active.release(&mut seat));
    unsafe { ffi::close(netlink) };
    result.and(release)
}

fn event_loop(
    seat: &mut Seat,
    netlink: i32,
    active: &mut Option<Active>,
    scene: &mut Scene,
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
            deadline_timeout(render_due, resize_due, now)
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
                    let current = active.take().ok_or(())?;
                    current.release(seat)?;
                    seat.acknowledge_disable()?;
                    render_due = None;
                    resize_due = None;
                    prepared_resize.take();
                    flip_requested = false;
                }
            }
        }
        let hotplug = if descriptors[4].returned & ffi::POLLIN != 0 {
            drain_hotplug(netlink)?
        } else {
            false
        };
        let Some(current) = active.as_mut() else {
            continue;
        };
        for descriptor in &descriptors[1..4] {
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
        if hotplug {
            prepared_resize.take();
            resize_due = Some(now.saturating_add(RESIZE_QUIET_MS));
        }
        if resize_due.is_some_and(|deadline| deadline <= now) {
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
        if render_due.is_some_and(|deadline| deadline <= now) {
            let presented = if flip_requested {
                current.display.present_flip(scene)?
            } else {
                current.display.present_damage(scene)?
            };
            if presented {
                last_present = now;
                render_due = None;
                flip_requested = false;
            } else if if flip_requested {
                current.display.has_flip_damage()
            } else {
                current.display.has_active_damage()
            } {
                render_due = Some(now.saturating_add(FRAME_INTERVAL_MS));
            } else {
                render_due = None;
            }
        }
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
