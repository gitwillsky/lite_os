//! 系统首选播放设备由一份标准 ALSA 配置拥有，桌面原生 PCM 客户端读取同一份配置。

use std::{
    fs,
    io::{self, Write},
    path::Path,
};

const PATH: &str = "/etc/alsa/conf.d/90-liteos-playback.conf";
const KEY: &str = "liteos_playback_card ";

/// 已保存的 ALSA card ID；没有设置时使用系统初始声卡。
pub fn id() -> Option<String> {
    let text = fs::read_to_string(PATH).ok()?;
    let id = text
        .lines()
        .next()?
        .strip_prefix(KEY)?
        .strip_prefix('"')?
        .strip_suffix('"')?;
    Some(id.to_owned())
}

pub(super) fn card() -> io::Result<u32> {
    let Some(id) = id() else {
        return Ok(0);
    };
    super::playback_outputs()?
        .into_iter()
        .find(|output| output.id == id)
        .map(|output| output.card)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "首选设备已不可用"))
}

/// 原子发布当前可用的共享端点为 ALSA 默认播放和音量设备。
///
/// # Errors
///
/// 端点已不存在、配置目录不可写或配置发布失败时保留旧文件。
pub fn set(id: &str) -> io::Result<()> {
    if id.is_empty()
        || !id.bytes().all(|byte| byte.is_ascii_alphanumeric())
        || !super::playback_outputs()?
            .iter()
            .any(|output| output.id == id)
    {
        return Err(io::Error::new(io::ErrorKind::NotFound, "首选设备已不可用"));
    }
    let path = Path::new(PATH);
    fs::create_dir_all(path.parent().expect("ALSA configuration directory"))?;
    let temporary = path.with_extension(format!("{}.new", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| {
        writeln!(file, "{KEY}\"{id}\"")?;
        writeln!(
            file,
            "pcm.!default {{ type plug slave.pcm {{ type hw card {{ @func refer name liteos_playback_card }} device 0 }} }}"
        )?;
        writeln!(
            file,
            "ctl.!default {{ type hw card {{ @func refer name liteos_playback_card }} }}"
        )?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
