//! LiteOS terminal emulator and desktop display client.

mod atlas;
mod client;
mod configure;
mod input;
mod model;
mod pointer;
mod render;
mod session;

fn main() {
    std::panic::set_hook(Box::new(|_| eprintln!("terminal: invariant failure")));
    let Some(command) = startup_command() else {
        std::process::exit(1);
    };
    std::process::exit(client::run(&command));
}

fn startup_command() -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStringExt;

    let mut command = Vec::new();
    for argument in std::env::args_os().skip(1) {
        let argument = argument.into_vec();
        let separator = usize::from(!command.is_empty());
        command
            .try_reserve(separator.checked_add(argument.len())?)
            .ok()?;
        if separator != 0 {
            command.push(b' ');
        }
        command.extend_from_slice(&argument);
    }
    Some(command)
}
