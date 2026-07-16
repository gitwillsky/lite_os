use core::ptr;

use crate::{
    atlas::{Atlas, FontMetrics},
    display::{CandidateBuffer, Display, DisplayError},
    ffi::{self, DrmMode, PollFd, SockaddrNl},
    model::{Grid, Model, ResizeCandidate},
};

mod evdev;
mod input;
mod pointer;
mod session;

use input::{
    InputQueue, KeyboardState, MAX_KEY_BYTES, PTY_REPLY_EXPANSION, flush_input, open_keyboard,
    read_keyboard,
};
use pointer::{MAX_POINTER_BYTES, Pointer};
use session::{read_pty, replay_boot_log, set_window_size, spawn_shell};

const FRAME_INTERVAL_MS: u64 = 17;
const BLINK_INTERVAL_MS: u64 = 500;
const RESIZE_QUIET_MS: u64 = 50;
// VirtIO display-info completion 没有独立 userspace edge；瞬时 EBUSY 后若不做有界重试，
// 最后一个重复 config event 会让 active mode 永久落后于 preferred mode。
const RESIZE_RETRY_MS: u64 = FRAME_INTERVAL_MS;

enum ResizeFailure {
    Transient,
    Rejected(DrmMode, DisplayError),
    Fatal,
}

/// 一次尚未对外发布的 resize transaction。
///
/// reactor 是这些候选对象的唯一 owner；瞬时 DRM busy 时保留它们可避免重复申请大块
/// framebuffer，mode 换代时整体丢弃则避免把旧网格提交到新 connector generation。
struct PreparedResize {
    mode: DrmMode,
    columns: usize,
    rows: usize,
    model: ResizeCandidate,
    buffer: CandidateBuffer,
}

pub fn run() -> Result<(), ()> {
    let atlas = Atlas::checked().ok_or(())?;
    let mut display = Display::open().map_err(|_| ())?;
    // 先订阅再查询初始 mode；否则 host resize 可消失在 query→event-loop 窗口，
    // userspace 会永久提交一个已经过期的 connector mode。
    let netlink = open_hotplug().ok_or(())?;
    let mode = display.query_mode().map_err(|_| ())?;
    let metrics = atlas.metrics();
    let mut model = Model::new(
        usize::from(mode.hdisplay) / metrics.width(),
        usize::from(mode.vdisplay) / metrics.height(),
    )
    .ok_or(())?;
    model.feed(b"\x1b[2J\x1b[HLiteOS\n\n", |_| {});
    replay_boot_log(&mut model);
    model.feed(b"[ OK ] DRM/KMS display session acquired\n", |_| {});

    let keyboard = open_keyboard();
    let mut pointer = Pointer::open();
    if keyboard >= 0 {
        model.feed(b"[ OK ] Keyboard input ready\n", |_| {});
    } else {
        model.feed(b"[WARN] Keyboard input unavailable\n", |_| {});
    }
    model.feed(b"[....] Starting shell\n\n", |_| {});
    let initial = display
        .prepare(mode, &model, &atlas, metrics)
        .map_err(|_| ())?;
    let mut initial = initial;
    display.commit(&mut initial).map_err(|_| ())?;
    model.clear_all_dirty();

    let (master, child) = spawn_shell(model.columns(), model.rows(), mode).ok_or(())?;
    model.begin_shell_session();
    let result = match display.present(&mut model, &atlas, metrics) {
        Ok(()) => event_loop(
            &mut display,
            &atlas,
            &mut model,
            metrics,
            master,
            keyboard,
            &mut pointer,
            netlink,
        ),
        Err(_) => Err(()),
    };
    unsafe {
        ffi::close(master);
        ffi::close(netlink);
        if keyboard >= 0 {
            ffi::close(keyboard);
        }
        ffi::waitpid(child, ptr::null_mut(), 0);
    }
    result
}

fn event_loop(
    display: &mut Display,
    atlas: &Atlas,
    model: &mut Model,
    metrics: FontMetrics,
    master: i32,
    keyboard: i32,
    pointer: &mut Option<Pointer>,
    netlink: i32,
) -> Result<(), ()> {
    let mut keyboard_state = KeyboardState::default();
    let mut input = InputQueue::new();
    let mut render_due = None::<u64>;
    let mut blink_due = None::<u64>;
    let mut resize_due = None::<u64>;
    let mut prepared_resize = None::<PreparedResize>;
    let mut last_present = ffi::monotonic_milliseconds();
    let mut warned_mode = None::<(u16, u16, u8)>;
    loop {
        let now = ffi::monotonic_milliseconds();
        if render_due.is_some_and(|deadline| deadline <= now) {
            display.present(model, atlas, metrics).map_err(|_| ())?;
            render_due = None;
            last_present = now;
        }
        if resize_due.is_some_and(|deadline| deadline <= now) {
            resize_due = None;
            match resize(display, atlas, model, metrics, master, &mut prepared_resize) {
                Ok(()) => warned_mode = None,
                Err(ResizeFailure::Transient) => {
                    resize_due = Some(now.saturating_add(RESIZE_RETRY_MS));
                }
                Err(ResizeFailure::Rejected(mode, error)) => {
                    let kind = match error {
                        DisplayError::OverBudget => 1,
                        DisplayError::OutOfMemory => 2,
                        DisplayError::System => 3,
                        DisplayError::Transient => unreachable!(),
                    };
                    let key = (mode.hdisplay, mode.vdisplay, kind);
                    if warned_mode != Some(key) {
                        warned_mode = Some(key);
                        report_resize_failure(error);
                    }
                }
                Err(ResizeFailure::Fatal) => return Err(()),
            }
        }

        if blink_due.is_some_and(|deadline| deadline <= now) {
            if model.toggle_blink() {
                schedule_render(&mut render_due, last_present, now);
                blink_due = Some(now.saturating_add(BLINK_INTERVAL_MS));
            } else {
                blink_due = None;
            }
        }

        let timeout = timeout(
            render_due,
            resize_due,
            blink_due,
            ffi::monotonic_milliseconds(),
        );
        let mut descriptors = [
            PollFd {
                fd: master,
                events: if input.remaining() >= PTY_REPLY_EXPANSION {
                    ffi::POLLIN
                } else {
                    0
                } | if input.is_empty() { 0 } else { ffi::POLLOUT },
                returned: 0,
            },
            PollFd {
                fd: netlink,
                events: ffi::POLLIN,
                returned: 0,
            },
            PollFd {
                fd: keyboard,
                events: if input.remaining() >= MAX_KEY_BYTES {
                    ffi::POLLIN
                } else {
                    0
                },
                returned: 0,
            },
            PollFd {
                fd: pointer.as_ref().map_or(-1, Pointer::fd),
                events: if input.remaining() >= MAX_POINTER_BYTES {
                    ffi::POLLIN
                } else {
                    0
                },
                returned: 0,
            },
        ];
        let count = 4;
        let ready = loop {
            let result = unsafe { ffi::poll(descriptors.as_mut_ptr(), count, timeout) };
            if result < 0 && ffi::errno() == ffi::EINTR {
                continue;
            }
            break result;
        };
        if ready < 0 {
            return Err(());
        }
        let now = ffi::monotonic_milliseconds();
        let mut closed = false;
        if descriptors[0].returned & (ffi::POLLIN | ffi::POLLERR | ffi::POLLHUP) != 0 {
            let (changed, ended) = read_pty(master, model, &mut input);
            // Device-status/attributes 查询属于 PTY request/reply；同轮立即写回可避免全屏程序
            // 等待下一次 POLLOUT edge 或超时，键盘/鼠标输入仍与回复共用唯一有界队列。
            flush_input(master, &mut input);
            closed = ended;
            if changed {
                schedule_render(&mut render_due, last_present, now);
                if blink_due.is_none() && model.has_blinking_cells() {
                    blink_due = Some(now.saturating_add(BLINK_INTERVAL_MS));
                }
            }
        }
        if descriptors[0].returned & ffi::POLLOUT != 0 {
            flush_input(master, &mut input);
        }
        if descriptors[1].returned & ffi::POLLIN != 0 && drain_hotplug(netlink) {
            resize_due = Some(now.saturating_add(RESIZE_QUIET_MS));
        }
        if keyboard >= 0 && descriptors[2].returned & ffi::POLLIN != 0 {
            read_keyboard(keyboard, &mut input, &mut keyboard_state, model);
            flush_input(master, &mut input);
        }
        if descriptors[3].returned & ffi::POLLIN != 0
            && let Some(pointer) = pointer.as_mut()
        {
            pointer.read(&mut input, model);
            flush_input(master, &mut input);
        }
        if render_due.is_some_and(|deadline| deadline <= now) {
            display.present(model, atlas, metrics).map_err(|_| ())?;
            render_due = None;
            last_present = now;
        }
        if closed {
            return Ok(());
        }
    }
}

fn resize(
    display: &mut Display,
    atlas: &Atlas,
    model: &mut Model,
    metrics: FontMetrics,
    master: i32,
    prepared: &mut Option<PreparedResize>,
) -> Result<(), ResizeFailure> {
    let mode = match display.query_mode() {
        Ok(mode) => mode,
        Err(DisplayError::Transient) => return Err(ResizeFailure::Transient),
        Err(error) => {
            prepared.take();
            return Err(ResizeFailure::Rejected(DrmMode::default(), error));
        }
    };
    if display.mode().is_some_and(|active| same_mode(active, mode)) {
        prepared.take();
        return Ok(());
    }
    if prepared
        .as_ref()
        .is_some_and(|candidate| !same_mode(candidate.mode, mode))
    {
        prepared.take();
    }
    if prepared.is_none() {
        let columns = usize::from(mode.hdisplay) / metrics.width();
        let rows = usize::from(mode.vdisplay) / metrics.height();
        if columns == 0 || rows == 0 {
            return Err(ResizeFailure::Rejected(mode, DisplayError::System));
        }
        let candidate_model = model
            .prepare_resize(columns, rows)
            .ok_or(ResizeFailure::Rejected(mode, DisplayError::OutOfMemory))?;
        let candidate_buffer = display
            .prepare(mode, &candidate_model, atlas, metrics)
            .map_err(|error| resize_error(mode, error))?;
        *prepared = Some(PreparedResize {
            mode,
            columns,
            rows,
            model: candidate_model,
            buffer: candidate_buffer,
        });
        let confirmed = match display.query_mode() {
            Ok(mode) => mode,
            Err(DisplayError::Transient) => return Err(ResizeFailure::Transient),
            Err(error) => {
                prepared.take();
                return Err(ResizeFailure::Rejected(mode, error));
            }
        };
        if !same_mode(mode, confirmed) {
            prepared.take();
            return Err(ResizeFailure::Transient);
        }
    }
    let result = display.commit(&mut prepared.as_mut().unwrap().buffer);
    if let Err(error) = result {
        if error == DisplayError::Transient {
            return Err(ResizeFailure::Transient);
        }
        prepared.take();
        return Err(ResizeFailure::Rejected(mode, error));
    }
    let candidate = prepared.take().unwrap();
    model.commit_resize(candidate.model);
    model.clear_all_dirty();
    // SETCRTC 已对外可见；此时回滚 PTY size 会与下一次 hotplug 竞争，因此失败后退出，
    // 由 init 从单一事实源完整重建 session。
    set_window_size(
        master,
        candidate.columns,
        candidate.rows,
        candidate.mode.hdisplay,
        candidate.mode.vdisplay,
    )
    .map_err(|()| ResizeFailure::Fatal)
}

fn resize_error(mode: DrmMode, error: DisplayError) -> ResizeFailure {
    if error == DisplayError::Transient {
        ResizeFailure::Transient
    } else {
        ResizeFailure::Rejected(mode, error)
    }
}

fn same_mode(first: DrmMode, second: DrmMode) -> bool {
    first.hdisplay == second.hdisplay && first.vdisplay == second.vdisplay
}

fn report_resize_failure(error: DisplayError) {
    let message: &[u8] = match error {
        DisplayError::Transient => return,
        DisplayError::OverBudget => {
            b"liteos-terminal: resize exceeds framebuffer budget; preserving active mode\n"
        }
        DisplayError::OutOfMemory => {
            b"liteos-terminal: resize out of memory; preserving active mode\n"
        }
        DisplayError::System => {
            b"liteos-terminal: resize transaction failed; preserving active mode\n"
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

fn schedule_render(deadline: &mut Option<u64>, last_present: u64, now: u64) {
    if deadline.is_none() {
        *deadline = Some(if now.saturating_sub(last_present) >= FRAME_INTERVAL_MS {
            now
        } else {
            last_present.saturating_add(FRAME_INTERVAL_MS)
        });
    }
}

fn timeout(render: Option<u64>, resize: Option<u64>, blink: Option<u64>, now: u64) -> i32 {
    let deadline = [render, resize, blink].into_iter().flatten().min();
    deadline.map_or(-1, |deadline| {
        i32::try_from(deadline.saturating_sub(now)).unwrap_or(i32::MAX)
    })
}

fn open_hotplug() -> Option<i32> {
    let fd = unsafe {
        ffi::socket(
            i32::from(ffi::AF_NETLINK),
            ffi::SOCK_DGRAM | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
            ffi::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return None;
    }
    let address = SockaddrNl {
        family: ffi::AF_NETLINK,
        padding: 0,
        port_id: 0,
        groups: 1,
    };
    if unsafe { ffi::bind(fd, &address, core::mem::size_of::<SockaddrNl>() as u32) } < 0 {
        unsafe { ffi::close(fd) };
        return None;
    }
    Some(fd)
}

fn drain_hotplug(fd: i32) -> bool {
    let mut received = false;
    let mut bytes = [0u8; 512];
    loop {
        let count = unsafe { ffi::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count > 0 {
            received = true;
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else {
            return received;
        }
    }
}

pub(super) fn decimal(mut value: u32, output: &mut [u8]) -> usize {
    let mut reversed = [0u8; 10];
    let mut length = 0;
    loop {
        reversed[length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for index in 0..length {
        output[index] = reversed[length - index - 1];
    }
    length
}
