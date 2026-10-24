//! LiteOS 显示服务：唯一的 DRM master，也是 Wayland 服务端。

mod boot;
mod compose;
mod cursor;
mod desktop;
mod input;
mod scanout;
mod session;
mod wayland;

use std::{env, fs, path::Path, process::ExitCode};

const DRM_DEVICE: &str = "/dev/dri/card0";
const INPUT_DIRECTORY: &str = "/dev/input";
const BOOT_SCENE: &str = "/usr/share/liteos/boot-scene.lbs";
const CURSORS: &str = "/usr/share/liteos/cursors.lcs";
/// 用户主题覆盖文件；不存在时使用内置默认。
const THEME_OVERRIDE: &str = "/etc/liteos/classic.theme";
const SOCKET_NAME: &str = "wayland-0";
/// 逻辑桌面高度不低于 768 的最大整数 scale（见外观规格 HiDPI）。
const MINIMUM_LOGICAL_HEIGHT: usize = 768;

fn output_scale(height: usize) -> u32 {
    (height / MINIMUM_LOGICAL_HEIGHT).max(1) as u32
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let read = |path: &str| fs::read(path).map_err(|error| format!("{path}: {error}"));
    let boot_scene = read(BOOT_SCENE)?;
    let cursors = cursor::CursorSet::parse(&read(CURSORS)?)?;
    let runtime = env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    let theme = classic_theme::Theme::load(&[Path::new(THEME_OVERRIDE)])?;

    let mut scanout =
        scanout::Scanout::open(DRM_DEVICE).map_err(|error| format!("{DRM_DEVICE}: {error}"))?;
    boot::paint(scanout.back(), &boot_scene, boot::HANDOFF_PERMILLE)?;
    scanout.show_first()?;
    println!(
        "compositor: first frame presented {}x{}",
        scanout.width(),
        scanout.height()
    );

    let scale = output_scale(scanout.height());
    let input = input::Input::open(
        Path::new(INPUT_DIRECTORY),
        scanout.width() as i32,
        scanout.height() as i32,
    )?;
    let arrow = cursors.get("arrow").ok_or("cursor set has no `arrow`")?;
    let mut hardware = scanout
        .device()
        .create_dumb(cursor::HARDWARE_EDGE as u32, cursor::HARDWARE_EDGE as u32)?;
    let (edge, frame) = arrow.frame(scale, 0);
    cursor::blit_to_hardware(&mut hardware, edge, frame)?;
    let hotspot = arrow.hotspot(scale);
    scanout.show_cursor(&hardware, input.pointer().position(), hotspot)?;
    println!(
        "compositor: input ready devices={} scale={scale}",
        input.devices().count()
    );

    let socket = Path::new(&runtime).join(SOCKET_NAME);
    let server = wayland::Server::bind(
        &socket,
        wayland::Output {
            width: scanout.width() as i32,
            height: scanout.height() as i32,
            scale: scale as i32,
        },
    )
    .map_err(|error| format!("{}: {error}", socket.display()))?;
    println!("compositor: wayland ready {}", socket.display());

    Ok(session::Session {
        scanout,
        input,
        server,
        theme,
        scale,
        _cursor: hardware,
    }
    .run()?)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("compositor: error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scale_follows_the_minimum_logical_height_rule() {
        for (height, scale) in [
            (480, 1),
            (768, 1),
            (800, 1),
            (1080, 1),
            (1536, 2),
            (1692, 2),
            (2160, 2),
            (2304, 3),
        ] {
            assert_eq!(super::output_scale(height), scale, "{height}");
        }
    }
}
