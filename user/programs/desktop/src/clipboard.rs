//! 桌面文件粘贴。Wayland 选择由 UI 线程核对，磁盘操作由单个后台任务完成。
use classic_ui::Display;
use desktop_entry::{file_uris, fileops};
use std::{
    io::{self, Read},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_PATHS: usize = 4096;
// 文件管理器格式比 URI 列表多一个剪切或复制的动词行。
const LIMIT: usize = file_uris::MAX_BYTES + "copy\n".len();

fn selection(display: &Display) -> Option<(&'static str, u64)> {
    file_uris::SELECTION_TYPES.into_iter().find_map(|mime| {
        display
            .selection_revision(mime)
            .map(|revision| (mime, revision))
    })
}

pub(super) fn can_paste(display: &Display) -> bool {
    selection(display).is_some()
}

pub(super) fn offer(
    display: &mut Display,
    paths: &[PathBuf],
    cut: bool,
    revision: Option<u64>,
) -> io::Result<bool> {
    let formats = if paths.is_empty() {
        Vec::new()
    } else {
        file_uris::encode_selection(paths, cut, MAX_PATHS)?
            .into_iter()
            .map(|(mime, data)| (mime.into(), data.into()))
            .collect()
    };
    display.set_selection(formats, revision)
}

/// 只有一项桌面粘贴可进行。这样连续按键不会重复复制或竞争移动同一批源文件。
pub(super) enum Paste {
    Reading {
        mime: &'static str,
        revision: u64,
        stream: UnixStream,
        bytes: Vec<u8>,
        deadline: Instant,
    },
    Transferring {
        cut_revision: Option<u64>,
        job: JoinHandle<fileops::Transfer>,
    },
}

impl Paste {
    pub(super) fn begin(display: &mut Display) -> io::Result<Option<Self>> {
        let Some((mime, revision)) = selection(display) else {
            return Ok(None);
        };
        Ok(display
            .receive_selection(mime)?
            .map(|stream| Self::Reading {
                mime,
                revision,
                stream,
                bytes: Vec::new(),
                deadline: Instant::now() + Duration::from_secs(2),
            }))
    }

    /// 推进一次；未完成则交回状态。超时、损坏内容、I/O 或后台任务失败时返回错误。
    pub(super) fn poll(self, display: &mut Display, home: &Path) -> io::Result<Option<Self>> {
        match self {
            // 关闭菜单与桌面重新取得焦点可能分批到达，允许原请求在期限内继续等待。
            waiting @ Self::Reading { deadline, .. }
                if display.keyboard_focus().is_none() && Instant::now() < deadline =>
            {
                Ok(Some(waiting))
            }
            Self::Reading {
                mime,
                revision,
                mut stream,
                mut bytes,
                deadline,
            } => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "读取剪贴板超时"));
                }
                if display.selection_revision(mime) != Some(revision) {
                    return Ok(None);
                }
                let mut chunk = [0; 8192];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(count) if bytes.len() + count <= LIMIT => {
                            bytes.extend_from_slice(&chunk[..count])
                        }
                        Ok(_) => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "剪贴板文件列表过长",
                            ));
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) =>
                        {
                            return Ok(Some(Self::Reading {
                                mime,
                                revision,
                                stream,
                                bytes,
                                deadline,
                            }));
                        }
                        Err(error) => return Err(error),
                    }
                }
                let (cut, paths) = file_uris::decode_selection(mime, &bytes, MAX_PATHS)?;
                let home = home.to_owned();
                let job = thread::Builder::new()
                    .name("desktop-paste".into())
                    .spawn(move || fileops::transfer(&home, paths, &home.join("Desktop"), cut))?;
                Ok(Some(Self::Transferring {
                    cut_revision: cut.then_some(revision),
                    job,
                }))
            }
            Self::Transferring { cut_revision, job } if job.is_finished() => {
                let outcome = job
                    .join()
                    .map_err(|_| io::Error::other("后台文件操作失败"))?;
                let selection_update = if let Some(revision) = cut_revision
                    && outcome.last.is_some()
                {
                    // 只替换仍在焦点内的原选择，保留失败项；用户期间复制的新内容优先。
                    offer(display, &outcome.remaining, true, Some(revision)).map(drop)
                } else {
                    Ok(())
                };
                outcome.result?;
                selection_update?;
                Ok(None)
            }
            waiting => Ok(Some(waiting)),
        }
    }
}
