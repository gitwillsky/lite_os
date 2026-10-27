//! 用户图片的异步加载；UI 只提交引用集合和借用已完成的像素，不执行文件 I/O 或解码。
use crate::Bitmap;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    thread,
};

enum Picture {
    Pending,
    Loading,
    // 失败也算完成；若不记录，损坏文件会在每次重绘时重新读盘解码。
    Loaded(Option<Bitmap>),
}

/// UI 线程拥有的用户图片缓存与单个后台解码线程。
///
/// 缓存只保留当前引用；任务和结果队列各容量一，任何时刻至多一项在途。
/// Drop 关闭通道，不等待已经开始的文件读取；后台仅转移 Bitmap，不接触窗口或 Wayland。
#[derive(Default)]
pub struct PictureLoader {
    pictures: BTreeMap<PathBuf, Picture>,
    worker: Option<Worker>,
    // 在途路径不随引用撤销而清除，否则快速切图会积压后台任务及大幅图片结果。
    active: Option<PathBuf>,
}

impl PictureLoader {
    /// 更新引用、接收完成结果并调度下一张图片；所有通道操作均不阻塞 UI。
    ///
    /// # 参数
    /// - `paths`：当前全部消费者引用的路径；不再引用的图片立即从缓存移除。
    ///
    /// # 返回值
    /// 当前引用的图片有新结果时返回 true，调用方应使消费者绘制失效。
    ///
    /// # 错误
    /// 创建线程、唤醒 socket 或后台通道失败时返回错误；文件/解码失败只记录一次并保持空白。
    pub fn update(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> io::Result<bool> {
        // 1. 先撤销引用，再处理完成通知。Loading 被移除后重新引用同一路径会成为 Pending，
        //    旧结果不能覆盖它；单项在途保证在旧结果回收前不会出现第二个同路径 Loading。
        let paths: BTreeSet<_> = paths.into_iter().collect();
        self.pictures.retain(|path, _| paths.contains(path));
        for path in paths {
            self.pictures.entry(path).or_insert(Picture::Pending);
        }

        // 2. 唤醒字节只是 readiness，完成通道才是结果 owner；两者先后被消费也不会丢失结果。
        let mut changed = false;
        if let Some(worker) = &mut self.worker {
            worker.drain_wake()?;
            match worker.receiver.try_recv() {
                Ok((path, result)) => {
                    debug_assert_eq!(self.active.as_ref(), Some(&path));
                    self.active = None;
                    if let Some(picture @ Picture::Loading) = self.pictures.get_mut(&path) {
                        *picture = Picture::Loaded(
                            result
                                .inspect_err(|error| {
                                    eprintln!("classic-ui: picture {}: {error}", path.display());
                                })
                                .ok(),
                        );
                        changed = true;
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    return Err(io::Error::other("picture worker disconnected"));
                }
            }
        }

        // 3. 只向空闲线程发送当前仍需要的下一项；待选路径留在 UI，不把过期选择排进工作队列。
        if self.active.is_none()
            && let Some((path, picture)) = self
                .pictures
                .iter_mut()
                .find(|(_, picture)| matches!(picture, Picture::Pending))
        {
            if self.worker.is_none() {
                self.worker = Some(Worker::new()?);
            }
            self.worker
                .as_ref()
                .expect("picture worker created")
                .sender
                .try_send(path.clone())
                .map_err(|error| io::Error::other(error.to_string()))?;
            *picture = Picture::Loading;
            self.active = Some(path.clone());
        }
        Ok(changed)
    }

    /// 借用已就绪图片；不触发加载或等待。
    ///
    /// # 参数
    /// - `path`：消费者当前引用的路径。
    ///
    /// # 返回值
    /// 加载成功时返回位图；未引用、等待中或解码失败时返回 None。
    pub fn get(&self, path: &Path) -> Option<&Bitmap> {
        match self.pictures.get(path) {
            Some(Picture::Loaded(image)) => image.as_ref(),
            _ => None,
        }
    }

    /// 返回应加入 UI poll 的可读通知源；尚未启动线程时返回 None。
    ///
    /// FD 只用于等待，调用方不得读取；唤醒后调用 [`Self::update`] 回收结果。
    pub fn notification(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.worker.as_ref().map(|worker| worker.reader.as_fd())
    }
}

struct Worker {
    sender: SyncSender<PathBuf>,
    receiver: Receiver<(PathBuf, io::Result<Bitmap>)>,
    reader: UnixStream,
}

impl Worker {
    fn new() -> io::Result<Self> {
        let (reader, mut writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        let (sender, jobs) = mpsc::sync_channel::<PathBuf>(1);
        let (results, receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("picture-decode".into())
            .spawn(move || {
                // OWNER: 线程只持有路径和独占位图；UI Drop 先断开请求/结果端，无 join、无 UI 回调。
                while let Ok(path) = jobs.recv() {
                    let result = Bitmap::load_picture(&path);
                    if results.send((path, result)).is_err() {
                        break;
                    }
                    loop {
                        match writer.write(&[1]) {
                            Ok(1) => break,
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            // 缓冲已满表示已有未消费通知；结果仍在有界通道，不需要额外唤醒。
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                            _ => return,
                        }
                    }
                }
            })?;
        Ok(Self {
            sender,
            receiver,
            reader,
        })
    }

    fn drain_wake(&mut self) -> io::Result<()> {
        let mut bytes = [0; 64];
        loop {
            match self.reader.read(&mut bytes) {
                Ok(0) => return Err(io::Error::other("picture wake socket closed")),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}
