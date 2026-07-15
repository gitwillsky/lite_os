use core::{ffi::c_void, ptr};

use crate::{
    atlas::{Atlas, FontMetrics},
    display::{CandidateBuffer, Display, DisplayError},
    ffi::{self, DrmMode, InputEvent, PollFd, SockaddrNl, WindowSize},
    model::{Grid, Model, ResizeCandidate},
};

const FRAME_INTERVAL_MS: u64 = 17;
const RESIZE_QUIET_MS: u64 = 50;
// VirtIO display-info completion 没有独立 userspace edge；瞬时 EBUSY 后若不做有界重试，
// 最后一个重复 config event 会让 active mode 永久落后于 preferred mode。
const RESIZE_RETRY_MS: u64 = FRAME_INTERVAL_MS;
const PTY_BUDGET: usize = 64 * 1024;
const INPUT_CAPACITY: usize = 4 * 1024;
const MAX_KEY_BYTES: usize = 4;

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
    model.feed(b"\x1b[2J\x1b[HLiteOS\n\n");
    replay_boot_log(&mut model);
    model.feed(b"[ OK ] DRM/KMS display session acquired\n");

    let keyboard = open_keyboard();
    if keyboard >= 0 {
        model.feed(b"[ OK ] Keyboard input ready\n");
    } else {
        model.feed(b"[WARN] Keyboard input unavailable\n");
    }
    model.feed(b"[....] Starting shell\n\n");
    let initial = display
        .prepare(mode, &model, &atlas, metrics)
        .map_err(|_| ())?;
    let mut initial = initial;
    display.commit(&mut initial).map_err(|_| ())?;
    model.clear_all_dirty();

    let (master, child) = spawn_shell(model.columns(), model.rows(), mode).ok_or(())?;
    let result = event_loop(
        &mut display,
        &atlas,
        &mut model,
        metrics,
        master,
        keyboard,
        netlink,
    );
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
    netlink: i32,
) -> Result<(), ()> {
    let mut keyboard_state = KeyboardState::default();
    let mut input = InputQueue::new();
    let mut render_due = None::<u64>;
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

        let timeout = timeout(render_due, resize_due, ffi::monotonic_milliseconds());
        let mut descriptors = [
            PollFd {
                fd: master,
                events: ffi::POLLIN | if input.is_empty() { 0 } else { ffi::POLLOUT },
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
        ];
        let count = if keyboard >= 0 { 3 } else { 2 };
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
            let (changed, ended) = read_pty(master, model);
            closed = ended;
            if changed {
                schedule_render(&mut render_due, last_present, now);
            }
        }
        if descriptors[0].returned & ffi::POLLOUT != 0 {
            flush_input(master, &mut input);
        }
        if descriptors[1].returned & ffi::POLLIN != 0 && drain_hotplug(netlink) {
            resize_due = Some(now.saturating_add(RESIZE_QUIET_MS));
        }
        if keyboard >= 0 && descriptors[2].returned & ffi::POLLIN != 0 {
            read_keyboard(keyboard, &mut input, &mut keyboard_state);
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

fn timeout(render: Option<u64>, resize: Option<u64>, now: u64) -> i32 {
    let deadline = match (render, resize) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    };
    deadline.map_or(-1, |deadline| {
        i32::try_from(deadline.saturating_sub(now)).unwrap_or(i32::MAX)
    })
}

fn read_pty(master: i32, model: &mut Model) -> (bool, bool) {
    let mut total = 0;
    let mut changed = false;
    let mut bytes = [0u8; 8 * 1024];
    while total < PTY_BUDGET {
        let capacity = bytes.len().min(PTY_BUDGET - total);
        let count = unsafe { ffi::read(master, bytes.as_mut_ptr().cast(), capacity) };
        if count > 0 {
            model.feed(&bytes[..count as usize]);
            total += count as usize;
            changed = true;
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else if count < 0 && ffi::errno() == ffi::EAGAIN {
            return (changed, false);
        } else {
            return (changed, true);
        }
    }
    (changed, false)
}

fn replay_boot_log(model: &mut Model) {
    let fd = unsafe {
        ffi::open(
            ffi::c_str(b"/dev/kmsg\0"),
            ffi::O_RDONLY | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return;
    }
    let mut record = [0u8; 256];
    loop {
        let count = unsafe { ffi::read(fd, record.as_mut_ptr().cast(), record.len()) };
        if count < 0 && ffi::errno() == ffi::EPIPE {
            continue;
        }
        if count <= 0 {
            break;
        }
        let bytes = &record[..count as usize];
        if let Some(separator) = bytes.iter().position(|byte| *byte == b';') {
            model.feed(&bytes[separator + 1..]);
            if bytes.last() != Some(&b'\n') {
                model.feed(b"\n");
            }
        }
    }
    unsafe { ffi::close(fd) };
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

fn spawn_shell(columns: usize, rows: usize, mode: DrmMode) -> Option<(i32, i32)> {
    let master = unsafe {
        ffi::open(
            ffi::c_str(b"/dev/ptmx\0"),
            ffi::O_RDWR | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
        )
    };
    if master < 0 {
        return None;
    }
    let mut index = 0u32;
    let mut unlocked = 0i32;
    if unsafe {
        ffi::ioctl(master, ffi::TIOCGPTN, (&mut index as *mut u32).cast()) < 0
            || ffi::ioctl(master, ffi::TIOCSPTLCK, (&mut unlocked as *mut i32).cast()) < 0
    } {
        unsafe { ffi::close(master) };
        return None;
    }
    let mut path = [0u8; 32];
    let prefix = b"/dev/pts/";
    path[..prefix.len()].copy_from_slice(prefix);
    let capacity = path.len() - 1;
    let length = prefix.len() + decimal(index, &mut path[prefix.len()..capacity]);
    path[length] = 0;
    let slave = unsafe { ffi::open(path.as_ptr().cast(), ffi::O_RDWR | ffi::O_CLOEXEC) };
    if slave < 0 || set_window_size(master, columns, rows, mode.hdisplay, mode.vdisplay).is_err() {
        unsafe {
            if slave >= 0 {
                ffi::close(slave);
            }
            ffi::close(master);
        }
        return None;
    }
    let child = unsafe { ffi::fork() };
    if child < 0 {
        unsafe {
            ffi::close(slave);
            ffi::close(master);
        }
        return None;
    }
    if child == 0 {
        unsafe {
            ffi::close(master);
            if ffi::setsid() < 0
                || ffi::ioctl(slave, ffi::TIOCSCTTY, ptr::null_mut()) < 0
                || ffi::dup2(slave, 0) < 0
                || ffi::dup2(slave, 1) < 0
                || ffi::dup2(slave, 2) < 0
            {
                ffi::_exit(126);
            }
            if slave > 2 {
                ffi::close(slave);
            }
            ffi::setenv(ffi::c_str(b"TERM\0"), ffi::c_str(b"linux\0"), 1);
            ffi::setenv(ffi::c_str(b"HOME\0"), ffi::c_str(b"/root\0"), 1);
            ffi::setenv(
                ffi::c_str(b"PATH\0"),
                ffi::c_str(b"/sbin:/usr/sbin:/bin:/usr/bin\0"),
                1,
            );
            ffi::chdir(ffi::c_str(b"/root\0"));
            let arguments = [ffi::c_str(b"-sh\0"), ptr::null()];
            ffi::execve(
                ffi::c_str(b"/bin/sh\0"),
                arguments.as_ptr(),
                ffi::environ.cast_const(),
            );
            ffi::_exit(127);
        }
    }
    unsafe { ffi::close(slave) };
    Some((master, child))
}

fn set_window_size(
    master: i32,
    columns: usize,
    rows: usize,
    pixel_width: u16,
    pixel_height: u16,
) -> Result<(), ()> {
    let mut size = WindowSize {
        rows: u16::try_from(rows).map_err(|_| ())?,
        columns: u16::try_from(columns).map_err(|_| ())?,
        pixel_width,
        pixel_height,
    };
    (unsafe {
        ffi::ioctl(
            master,
            ffi::TIOCSWINSZ,
            (&mut size as *mut WindowSize).cast(),
        )
    } >= 0)
        .then_some(())
        .ok_or(())
}

fn decimal(mut value: u32, output: &mut [u8]) -> usize {
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

fn open_keyboard() -> i32 {
    for index in 0..16u32 {
        let mut path = [0u8; 32];
        let prefix = b"/dev/input/event";
        path[..prefix.len()].copy_from_slice(prefix);
        let capacity = path.len() - 1;
        let length = prefix.len() + decimal(index, &mut path[prefix.len()..capacity]);
        path[length] = 0;
        let fd = unsafe {
            ffi::open(
                path.as_ptr().cast(),
                ffi::O_RDONLY | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
            )
        };
        if fd < 0 {
            continue;
        }
        let mut name = [0u8; 128];
        if unsafe { ffi::ioctl(fd, ffi::EVIOCGNAME_128, name.as_mut_ptr().cast()) } >= 0
            && contains_keyboard(&name)
        {
            let mut grab = 1i32;
            unsafe { ffi::ioctl(fd, ffi::EVIOCGRAB, (&mut grab as *mut i32).cast()) };
            return fd;
        }
        unsafe { ffi::close(fd) };
    }
    -1
}

fn contains_keyboard(name: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"keyboard";
    let mut matched = 0;
    for byte in name.iter().copied().take_while(|byte| *byte != 0) {
        let value = byte.to_ascii_lowercase();
        matched = if value == NEEDLE[matched] {
            matched + 1
        } else {
            usize::from(value == NEEDLE[0])
        };
        if matched == NEEDLE.len() {
            return true;
        }
    }
    false
}

#[derive(Default)]
struct KeyboardState {
    shift: bool,
    control: bool,
    alt: bool,
    caps_lock: bool,
}

fn read_keyboard(fd: i32, input: &mut InputQueue, state: &mut KeyboardState) {
    let mut events = [InputEvent {
        seconds: 0,
        microseconds: 0,
        kind: 0,
        code: 0,
        value: 0,
    }; 32];
    let capacity =
        events.len().min(input.remaining() / MAX_KEY_BYTES) * core::mem::size_of::<InputEvent>();
    if capacity == 0 {
        return;
    }
    let count = unsafe { ffi::read(fd, events.as_mut_ptr().cast(), capacity) };
    if count <= 0 {
        return;
    }
    for event in &events[..count as usize / core::mem::size_of::<InputEvent>()] {
        handle_key(input, state, event);
    }
}

fn handle_key(input: &mut InputQueue, state: &mut KeyboardState, event: &InputEvent) {
    if event.kind == 0 && event.code == 3 {
        // SYN_DROPPED 使此前 modifier snapshot 不再可信；清零可避免 Shift/Ctrl 永久粘住。
        *state = KeyboardState::default();
        return;
    }
    if event.kind != 1 {
        return;
    }
    let pressed = event.value != 0;
    match event.code {
        42 | 54 => {
            state.shift = pressed;
            return;
        }
        29 | 97 => {
            state.control = pressed;
            return;
        }
        56 | 100 => {
            state.alt = pressed;
            return;
        }
        58 => {
            if event.value == 1 {
                state.caps_lock = !state.caps_lock;
            }
            return;
        }
        _ => {}
    }
    if !pressed {
        return;
    }
    let sequence: &[u8] = match event.code {
        1 => b"\x1b",
        14 => b"\x7f",
        15 => b"\t",
        28 => b"\r",
        102 => b"\x1b[H",
        103 => b"\x1b[A",
        104 => b"\x1b[5~",
        105 => b"\x1b[D",
        106 => b"\x1b[C",
        107 => b"\x1b[F",
        108 => b"\x1b[B",
        109 => b"\x1b[6~",
        111 => b"\x1b[3~",
        _ => b"",
    };
    if !sequence.is_empty() {
        input.push(sequence);
        return;
    }
    let Some(mut character) = plain_key(event.code) else {
        return;
    };
    if character.is_ascii_alphabetic() {
        if state.shift != state.caps_lock {
            character = character.to_ascii_uppercase();
        }
    } else if state.shift {
        character = shifted_key(event.code).unwrap_or(character);
    }
    if state.control && character.is_ascii_lowercase() {
        character = character - b'a' + 1;
    }
    if state.alt {
        input.push(b"\x1b");
    }
    input.push(&[character]);
}

fn plain_key(code: u16) -> Option<u8> {
    Some(match code {
        2..=11 => *b"1234567890".get((code - 2) as usize)?,
        12 => b'-',
        13 => b'=',
        16..=27 => *b"qwertyuiop[]".get((code - 16) as usize)?,
        30..=41 => *b"asdfghjkl;'`".get((code - 30) as usize)?,
        43 => b'\\',
        44..=53 => *b"zxcvbnm,./".get((code - 44) as usize)?,
        57 => b' ',
        _ => return None,
    })
}

fn shifted_key(code: u16) -> Option<u8> {
    Some(match code {
        2..=13 => *b"!@#$%^&*()_+".get((code - 2) as usize)?,
        26 => b'{',
        27 => b'}',
        39 => b':',
        40 => b'"',
        41 => b'~',
        43 => b'|',
        51 => b'<',
        52 => b'>',
        53 => b'?',
        _ => return None,
    })
}

struct InputQueue {
    bytes: [u8; INPUT_CAPACITY],
    head: usize,
    length: usize,
}

impl InputQueue {
    const fn new() -> Self {
        Self {
            bytes: [0; INPUT_CAPACITY],
            head: 0,
            length: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.length == 0
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.length
    }

    fn push(&mut self, bytes: &[u8]) {
        assert!(bytes.len() <= self.remaining());
        for byte in bytes {
            let tail = (self.head + self.length) % self.bytes.len();
            self.bytes[tail] = *byte;
            self.length += 1;
        }
    }

    fn contiguous(&self) -> &[u8] {
        &self.bytes[self.head..self.head + self.length.min(self.bytes.len() - self.head)]
    }

    fn consume(&mut self, count: usize) {
        debug_assert!(count <= self.length);
        self.head = (self.head + count) % self.bytes.len();
        self.length -= count;
    }
}

fn flush_input(master: i32, input: &mut InputQueue) {
    while !input.is_empty() {
        let bytes = input.contiguous();
        let count = unsafe { ffi::write(master, bytes.as_ptr().cast::<c_void>(), bytes.len()) };
        if count > 0 {
            input.consume(count as usize);
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else {
            return;
        }
    }
}
