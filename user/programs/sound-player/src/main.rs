//! 播放一个系统提示音后退出：`sndplay <文件名>`，文件在 `/usr/share/liteos/sounds/` 下。
//! 独立进程，解码或设备出错只结束自己，不会拖垮调用它的桌面或资源管理器。
extern crate liteos_runtime as _;

use std::{path::Path, process::ExitCode};

fn main() -> ExitCode {
    let Some(name) = std::env::args().nth(1) else {
        eprintln!("sndplay: usage: sndplay <sound file name>");
        return ExitCode::FAILURE;
    };
    // 只接受文件名，避免通过 `..` 或绝对路径读取声音目录以外的文件。
    if Path::new(&name).components().count() != 1 || name.starts_with('.') {
        eprintln!("sndplay: invalid sound name");
        return ExitCode::FAILURE;
    }
    let path = Path::new(sound::DIRECTORY).join(&name);
    match sound::Sound::load(&path).and_then(|sound| sound.play_blocking()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sndplay: {}: {error}", path.display());
            ExitCode::FAILURE
        }
    }
}
