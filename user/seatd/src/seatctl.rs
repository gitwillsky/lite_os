//! seatctl：从 shell 请求切换座位，`seatctl text|desktop`。

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    process::ExitCode,
};

use seat_proto::{ClientFrame, FRAME_BYTES, Role, SOCKET_PATH, ServerFrame, SwitchResult};

fn main() -> ExitCode {
    let Some(role) = std::env::args().nth(1).as_deref().and_then(Role::parse) else {
        eprintln!("usage: seatctl text|desktop");
        return ExitCode::from(2);
    };
    match switch(role) {
        Ok(SwitchResult::Started | SwitchResult::AlreadyActive) => ExitCode::SUCCESS,
        Ok(SwitchResult::Unavailable) => {
            eprintln!("seatctl: that session is not running");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("seatctl: {error}");
            ExitCode::from(1)
        }
    }
}

fn switch(role: Role) -> std::io::Result<SwitchResult> {
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    stream.write_all(&ClientFrame::Switch(role).encode())?;
    let mut reply = [0u8; FRAME_BYTES];
    stream.read_exact(&mut reply)?;
    match ServerFrame::decode(reply) {
        Some(ServerFrame::Switched(result)) => Ok(result),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "seatd sent an unexpected frame",
        )),
    }
}
