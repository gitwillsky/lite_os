use crate::{
    ffi::{self, PollFd},
    input::{self, InputQueue, KeyboardState, MAX_KEY_BYTES},
    model::{Grid, Model},
    protocol::{Configuration, Connection, Event, GRID_CAPACITY},
    pty,
};

const FRAME_INTERVAL_MS: u64 = 17;
const BLINK_INTERVAL_MS: u64 = 500;
const EVENT_INPUT_RESERVE: usize = 64 * MAX_KEY_BYTES;

pub fn run() -> Result<(), ()> {
    let mut connection = Connection::connect()?;
    let configuration = wait_configuration(&mut connection)?;
    let mut model = Model::new(
        usize::from(configuration.columns),
        usize::from(configuration.rows),
    )
    .ok_or(())?;
    let (master, child) = pty::spawn_shell(
        model.columns(),
        model.rows(),
        configuration.pixel_width,
        configuration.pixel_height,
    )
    .ok_or(())?;
    model.begin_shell_session();
    let result = event_loop(&mut connection, master, &mut model, configuration);
    unsafe { ffi::close(master) };
    terminate_child(child);
    result
}

fn wait_configuration(connection: &mut Connection) -> Result<Configuration, ()> {
    loop {
        let mut descriptor = PollFd {
            fd: connection.fd(),
            events: ffi::POLLIN,
            returned: 0,
        };
        poll(core::slice::from_mut(&mut descriptor), -1)?;
        if descriptor.returned & (ffi::POLLERR | ffi::POLLHUP) != 0 {
            return Err(());
        }
        while let Some(event) = connection.read_event()? {
            if let Event::Configure(configuration) = event {
                validate_configuration(configuration)?;
                return Ok(configuration);
            }
        }
    }
}

fn event_loop(
    connection: &mut Connection,
    master: i32,
    model: &mut Model,
    mut configuration: Configuration,
) -> Result<(), ()> {
    let mut input = InputQueue::new();
    let mut keyboard = KeyboardState::default();
    let mut dirty = true;
    let mut render_due = Some(ffi::monotonic_milliseconds()?);
    let mut blink_due = None;
    let mut last_publish = 0;
    loop {
        let now = ffi::monotonic_milliseconds()?;
        if dirty && connection.can_publish() && render_due.is_some_and(|deadline| deadline <= now) {
            connection.queue_grid(model)?;
            dirty = false;
            render_due = None;
            last_publish = now;
        }
        let timeout = deadline_timeout(render_due, blink_due, now);
        let allow_events = input.remaining() >= EVENT_INPUT_RESERVE;
        let mut descriptors = [
            PollFd {
                fd: connection.fd(),
                events: connection.poll_events(allow_events),
                returned: 0,
            },
            PollFd {
                fd: master,
                events: ffi::POLLIN | if input.is_empty() { 0 } else { ffi::POLLOUT },
                returned: 0,
            },
        ];
        poll(&mut descriptors, timeout)?;
        if descriptors
            .iter()
            .any(|descriptor| descriptor.returned & (ffi::POLLERR | ffi::POLLHUP) != 0)
        {
            return Err(());
        }
        if descriptors[0].returned & ffi::POLLOUT != 0 {
            connection.flush()?;
        }
        if descriptors[0].returned & ffi::POLLIN != 0 {
            loop {
                let Some(event) = connection.read_event()? else {
                    break;
                };
                match event {
                    Event::Key { code, value } => {
                        input::handle_key(code, value, &mut input, &mut keyboard, model);
                    }
                    Event::Pointer {
                        button,
                        pressed,
                        column,
                        row,
                    } => input::handle_pointer(button, pressed, column, row, &mut input, model),
                    Event::Configure(next) if next != configuration => {
                        validate_configuration(next)?;
                        let candidate = model
                            .prepare_resize(usize::from(next.columns), usize::from(next.rows))
                            .ok_or(())?;
                        pty::set_window_size(
                            master,
                            usize::from(next.columns),
                            usize::from(next.rows),
                            next.pixel_width,
                            next.pixel_height,
                        )?;
                        model.commit_resize(candidate);
                        configuration = next;
                        dirty = true;
                    }
                    Event::Configure(_) => {}
                }
            }
            // Parser replies and keyboard bytes must attempt progress in the same poll turn;
            // otherwise an edge-triggered producer can wait forever for a future event.
            input::flush_input(master, &mut input);
        }
        if descriptors[1].returned & ffi::POLLIN != 0 {
            let (changed, closed) = pty::read_pty(master, model, &mut input);
            if closed {
                return Err(());
            }
            if changed {
                dirty = true;
                blink_due = model
                    .has_blinking_cells()
                    .then_some(now.saturating_add(BLINK_INTERVAL_MS));
            }
            input::flush_input(master, &mut input);
        }
        if descriptors[1].returned & ffi::POLLOUT != 0 {
            input::flush_input(master, &mut input);
        }
        let now = ffi::monotonic_milliseconds()?;
        if blink_due.is_some_and(|deadline| deadline <= now) {
            dirty |= model.toggle_blink();
            blink_due = Some(now.saturating_add(BLINK_INTERVAL_MS));
        }
        if dirty && render_due.is_none() {
            render_due = Some(last_publish.saturating_add(FRAME_INTERVAL_MS).max(now));
        }
    }
}

fn validate_configuration(configuration: Configuration) -> Result<(), ()> {
    let count = usize::from(configuration.columns)
        .checked_mul(usize::from(configuration.rows))
        .ok_or(())?;
    if count == 0
        || count > GRID_CAPACITY
        || configuration.pixel_width == 0
        || configuration.pixel_height == 0
    {
        Err(())
    } else {
        Ok(())
    }
}

fn poll(descriptors: &mut [PollFd], timeout: i32) -> Result<(), ()> {
    loop {
        let result = unsafe { ffi::poll(descriptors.as_mut_ptr(), descriptors.len(), timeout) };
        if result >= 0 {
            return Ok(());
        }
        if ffi::errno() != ffi::EINTR {
            return Err(());
        }
    }
}

fn deadline_timeout(render: Option<u64>, blink: Option<u64>, now: u64) -> i32 {
    [render, blink]
        .into_iter()
        .flatten()
        .min()
        .map_or(-1, |deadline| {
            i32::try_from(deadline.saturating_sub(now)).unwrap_or(i32::MAX)
        })
}

fn terminate_child(child: i32) {
    if child <= 0 {
        return;
    }
    unsafe { ffi::kill(child, ffi::SIGKILL) };
    loop {
        let mut status = 0;
        let result = unsafe { ffi::waitpid(child, &mut status, 0) };
        if result == child || result < 0 && ffi::errno() == ffi::ECHILD {
            return;
        }
        if result < 0 && ffi::errno() == ffi::EINTR {
            continue;
        }
        return;
    }
}
