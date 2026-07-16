use crate::{ffi, listener, process};

const DISPLAY_SESSION: &[u8] = b"/bin/display-session\0";
const COMPOSITOR: &[u8] = b"/bin/liteui-compositor\0";
const SYSTEM_SHELL: &[u8] = b"/bin/liteui-host\0";
const TERMINAL_SERVICE: &[u8] = b"/bin/terminal-service\0";
const CALCULATOR: &[u8] = b"/usr/lib/liteui/apps/calculator\0";
const RAPID_FAILURE_MS: u64 = 2_000;
const MAX_RAPID_FAILURES: u8 = 3;

struct Generation {
    broker: i32,
    compositor: i32,
    shell: Option<i32>,
    shell_started: u64,
    shell_failures: u8,
    terminal: Option<i32>,
    terminal_started: u64,
    terminal_failures: u8,
    calculator: Option<i32>,
    calculator_started: u64,
    calculator_failures: u8,
}

pub fn run() -> Result<(), ()> {
    let mut rapid_failures = 0u8;
    loop {
        let started = monotonic()?;
        let mut generation = Generation::start()?;
        let supervision = generation.supervise();
        // Supervision errors must cross the same teardown barrier as an observed
        // broker/compositor exit; returning early would orphan the remaining generation.
        generation.terminate();
        supervision?;
        rapid_failures = if monotonic()?.saturating_sub(started) < RAPID_FAILURE_MS {
            rapid_failures.saturating_add(1)
        } else {
            0
        };
        if rapid_failures >= MAX_RAPID_FAILURES {
            return Err(());
        }
    }
}

impl Generation {
    fn start() -> Result<Self, ()> {
        // Capture the supervision epoch before publishing any child. If the clock read
        // failed after fork, no owner would remain to reap the partially started generation.
        let now = monotonic()?;
        let seat = listener::create(listener::SEAT_PATH, 0, 0, 0o600)?;
        let broker = match process::spawn(
            DISPLAY_SESSION,
            None,
            Some((seat, b"display-session\0")),
            process::Identity::Root,
        ) {
            Ok(pid) => pid,
            Err(()) => {
                unsafe { ffi::close(seat) };
                listener::remove(listener::SEAT_PATH);
                return Err(());
            }
        };
        unsafe { ffi::close(seat) };

        let socket = match listener::create(listener::COMPOSITOR_PATH, 100, 100, 0o660) {
            Ok(fd) => fd,
            Err(()) => {
                process::terminate(broker);
                listener::remove(listener::SEAT_PATH);
                return Err(());
            }
        };
        let compositor = match process::spawn(
            COMPOSITOR,
            None,
            Some((socket, b"liteui-compositor\0")),
            process::Identity::Root,
        ) {
            Ok(pid) => pid,
            Err(()) => {
                unsafe { ffi::close(socket) };
                process::terminate(broker);
                listener::remove(listener::SEAT_PATH);
                listener::remove(listener::COMPOSITOR_PATH);
                return Err(());
            }
        };
        unsafe { ffi::close(socket) };
        let shell = match process::spawn(SYSTEM_SHELL, None, None, process::Identity::SystemShell) {
            Ok(pid) => pid,
            Err(()) => {
                process::terminate(compositor);
                process::terminate(broker);
                listener::remove(listener::SEAT_PATH);
                listener::remove(listener::COMPOSITOR_PATH);
                return Err(());
            }
        };
        let calculator = match process::spawn(
            SYSTEM_SHELL,
            Some(CALCULATOR),
            None,
            process::Identity::Application,
        ) {
            Ok(pid) => pid,
            Err(()) => {
                process::terminate(shell);
                process::terminate(compositor);
                process::terminate(broker);
                listener::remove(listener::SEAT_PATH);
                listener::remove(listener::COMPOSITOR_PATH);
                return Err(());
            }
        };
        let terminal =
            match process::spawn(TERMINAL_SERVICE, None, None, process::Identity::Terminal) {
                Ok(pid) => pid,
                Err(()) => {
                    process::terminate(calculator);
                    process::terminate(shell);
                    process::terminate(compositor);
                    process::terminate(broker);
                    listener::remove(listener::SEAT_PATH);
                    listener::remove(listener::COMPOSITOR_PATH);
                    return Err(());
                }
            };
        Ok(Self {
            broker,
            compositor,
            shell: Some(shell),
            shell_started: now,
            shell_failures: 0,
            terminal: Some(terminal),
            terminal_started: now,
            terminal_failures: 0,
            calculator: Some(calculator),
            calculator_started: now,
            calculator_failures: 0,
        })
    }

    fn supervise(&mut self) -> Result<(), ()> {
        loop {
            let pid = process::wait_any()?;
            if pid == self.broker || pid == self.compositor {
                return Ok(());
            }
            if self.shell == Some(pid) {
                restart(
                    &mut self.shell,
                    &mut self.shell_started,
                    &mut self.shell_failures,
                    SYSTEM_SHELL,
                    None,
                    process::Identity::SystemShell,
                )?;
            } else if self.calculator == Some(pid) {
                restart(
                    &mut self.calculator,
                    &mut self.calculator_started,
                    &mut self.calculator_failures,
                    SYSTEM_SHELL,
                    Some(CALCULATOR),
                    process::Identity::Application,
                )?;
            } else if self.terminal == Some(pid) {
                restart(
                    &mut self.terminal,
                    &mut self.terminal_started,
                    &mut self.terminal_failures,
                    TERMINAL_SERVICE,
                    None,
                    process::Identity::Terminal,
                )?;
            }
        }
    }

    fn terminate(&mut self) {
        if let Some(shell) = self.shell.take() {
            process::terminate(shell);
        }
        if let Some(calculator) = self.calculator.take() {
            process::terminate(calculator);
        }
        if let Some(terminal) = self.terminal.take() {
            process::terminate(terminal);
        }
        process::terminate(self.compositor);
        process::terminate(self.broker);
        listener::remove(listener::COMPOSITOR_PATH);
        listener::remove(listener::SEAT_PATH);
    }
}

fn restart(
    process: &mut Option<i32>,
    started: &mut u64,
    failures: &mut u8,
    binary: &'static [u8],
    argument: Option<&'static [u8]>,
    identity: process::Identity,
) -> Result<(), ()> {
    *process = None;
    let now = monotonic()?;
    *failures = if now.saturating_sub(*started) < RAPID_FAILURE_MS {
        failures.saturating_add(1)
    } else {
        0
    };
    if *failures >= MAX_RAPID_FAILURES {
        return Ok(());
    }
    *process = Some(process::spawn(binary, argument, None, identity)?);
    *started = now;
    Ok(())
}

fn monotonic() -> Result<u64, ()> {
    let mut value = ffi::Timespec {
        seconds: 0,
        nanoseconds: 0,
    };
    if unsafe { ffi::clock_gettime(ffi::CLOCK_MONOTONIC, &mut value) } != 0
        || value.seconds < 0
        || value.nanoseconds < 0
    {
        return Err(());
    }
    (value.seconds as u64)
        .checked_mul(1_000)
        .and_then(|seconds| seconds.checked_add(value.nanoseconds as u64 / 1_000_000))
        .ok_or(())
}
