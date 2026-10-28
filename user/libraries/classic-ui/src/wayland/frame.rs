//! wl_shm 缓冲池：Display 独占内存与 busy 状态，release 之前绝不重写。

use super::{Display, WindowId};
use canvas::Rect;
use linux_uapi::shared_memory::MemFd;
use std::{collections::VecDeque, io, os::fd::AsFd};
use wayland_wire::{
    protocol::{Interface, wl_buffer, wl_shm, wl_shm_pool, wl_surface},
    wire::{ObjectId, Writer},
};

/// 三个在途缓冲覆盖生产者/消费者交错，同时限制慢消费者占用的内存。
const WINDOW_BUFFER_LIMIT: usize = 3;

pub(super) struct Buffer {
    pub window: WindowId,
    pub size: (i32, i32),
    pub busy: bool,
    /// 内存里是第几次提交的完整画面；0 表示从未写入，复用时必须整幅写。
    commit: u64,
    memory: MemFd,
}

/// 一个窗口最近几次提交的损伤：复用的缓冲落后几帧，就补写这几帧的损伤。
#[derive(Default)]
pub(super) struct History {
    commits: u64,
    /// 最近 [`WINDOW_BUFFER_LIMIT`] 次提交的损伤，最新在后；缓冲最多落后这么多帧。
    damage: VecDeque<Vec<Rect>>,
    /// 历史对应的缓冲像素尺寸与 scale，也是上一次提交的内容规格；变化时历史作废。
    key: Option<(i32, i32, i32)>,
    /// 上一次提交的完整画面；新画面与它逐行比较得出损伤。缺少它只能整幅提交，
    /// 一次标题栏变色也要搬运整窗像素。只按损伤更新，不每帧整幅复制。
    frame: Vec<u32>,
}

impl History {
    /// 本次画面相对上一次提交的损伤；规格变化或首次提交为整幅。
    fn damage(&self, key: (i32, i32, i32), pixels: &[u32]) -> Vec<Rect> {
        if self.key == Some(key) && self.frame.len() == pixels.len() {
            changed(&self.frame, pixels, key.0 as usize)
        } else {
            vec![Rect::new(0, 0, key.0, key.1)]
        }
    }

    fn record(&mut self, key: (i32, i32, i32), pixels: &[u32], damage: Vec<Rect>) {
        if self.key != Some(key) || self.frame.len() != pixels.len() {
            self.damage.clear();
            self.key = Some(key);
            self.frame.clear();
            self.frame.extend_from_slice(pixels);
        } else {
            let width = key.0 as usize;
            for rect in &damage {
                for row in rect.y as usize..rect.bottom() as usize {
                    let start = row * width + rect.x as usize;
                    let end = start + rect.width as usize;
                    self.frame[start..end].copy_from_slice(&pixels[start..end]);
                }
            }
        }
        self.commits += 1;
        if self.damage.len() == WINDOW_BUFFER_LIMIT {
            self.damage.pop_front();
        }
        self.damage.push_back(damage);
    }

    /// 持有第 `held` 次提交的缓冲需要补写的损伤；`None` 表示必须整幅写。
    fn since(&self, held: u64, key: (i32, i32, i32)) -> Option<impl Iterator<Item = &Rect>> {
        let lag = self.commits.checked_sub(held)? as usize;
        (held != 0 && self.key == Some(key) && lag <= self.damage.len())
            .then(|| self.damage.iter().skip(self.damage.len() - lag).flatten())
    }
}

impl Display {
    pub(super) fn buffer_available(&self, window: WindowId) -> bool {
        let mut count = 0;
        for buffer in self
            .buffers
            .values()
            .filter(|buffer| buffer.window == window)
        {
            if !buffer.busy {
                return true;
            }
            count += 1;
        }
        count < WINDOW_BUFFER_LIMIT
    }

    /// 提交一帧预乘 ARGB 画面；尺寸来自当前 configure，缓冲复用必须等待 release。
    ///
    /// # Parameters
    ///
    /// - `pixels`: 完整画面，长度等于当前配置的像素数。与上一次提交逐行比较得出损伤，
    ///   只写入并声明变化区域；完全相同且已按同一规格映射时只提交 frame callback，不 attach 缓冲。
    ///
    /// # Errors
    /// 在途 callback/缓冲池满返回 WouldBlock；配置、尺寸、分配、编码与 I/O 失败保留错误。
    /// 连接错误后调用方应销毁 Display，不能继续使用部分发送的请求。
    pub fn present(&mut self, window: WindowId, pixels: &[u32]) -> io::Result<()> {
        let state = self
            .windows
            .get(&window)
            .filter(|state| state.configured)
            .ok_or_else(|| io::Error::other("window has not been configured"))?;
        if state.frame.is_some() || !self.buffer_available(window) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let configuration = state.configuration;
        let (width, height) = configuration.pixel_size()?;
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| io::Error::other("shm stride overflow"))?;
        let bytes = stride
            .checked_mul(height)
            .ok_or_else(|| io::Error::other("shm extent overflow"))?;
        if pixels.len() != bytes as usize / 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pixel count does not match window configuration",
            ));
        }
        let key = (width, height, configuration.scale);
        let bounds = Rect::new(0, 0, width, height);
        let mapped = state.mapped && state.history.key == Some(key);
        let damage = if mapped {
            state.history.damage(key, pixels)
        } else {
            vec![bounds]
        };
        let callback = self.transport.allocate(Interface::WlCallback)?;
        // 1. 画面没变且已按同一规格映射：只请求 frame 并 commit。configure 的 ack 要随 commit 生效，
        //    不能因为像素相同就省掉 commit；也不 attach，免得占用缓冲和触发合成器读取。
        if damage.is_empty() && mapped {
            let writer = &mut self.transport.writer;
            wl_surface::Request::Frame { callback }
                .encode(window.0, writer)
                .map_err(io::Error::other)?;
            wl_surface::Request::Commit
                .encode(window.0, writer)
                .map_err(io::Error::other)?;
            self.windows
                .get_mut(&window)
                .expect("validated window")
                .frame = Some(callback);
            return self.transport.flush();
        }
        // 2. 淘汰不匹配尺寸的空闲缓冲；仍 busy 的旧尺寸保持 owner，直到 release。
        let retired: Vec<_> = self
            .buffers
            .iter()
            .filter_map(|(id, buffer)| {
                (buffer.window == window && !buffer.busy && buffer.size != (width, height))
                    .then_some(*id)
            })
            .collect();
        for id in retired {
            self.buffers.remove(&id);
            wl_buffer::Request::Destroy
                .encode(id, &mut self.transport.writer)
                .map_err(io::Error::other)?;
        }
        let reusable = self
            .buffers
            .iter()
            .find_map(|(id, buffer)| (buffer.window == window && !buffer.busy).then_some(*id));
        let id = match reusable {
            Some(id) => id,
            None => self.allocate_buffer(window, (width, height), stride, bytes)?,
        };
        // 3. 复用的缓冲停在它上次提交的画面：补写之后各帧与本帧的损伤，新缓冲整幅写。
        let history = &self.windows[&window].history;
        let buffer = self.buffers.get(&id).expect("allocated buffer");
        let written: Vec<Rect> = match history.since(buffer.commit, key) {
            Some(lagging) => lagging.chain(&damage).copied().collect(),
            None => vec![bounds],
        };
        let row = width as usize;
        buffer
            .memory
            .write_segments(written.iter().flat_map(|rect| {
                let (x, y) = (rect.x as usize, rect.y as usize);
                let (columns, rows) = (rect.width as usize, rect.height as usize);
                // 宽矩形连同行间像素一次写入（写的是当前画面，多写也正确）；窄矩形逐行写，避免整段搬运。
                let segments: Vec<(usize, &[u32])> = if columns * 2 >= row {
                    let start = y * row + x;
                    vec![(start, &pixels[start..(y + rows - 1) * row + x + columns])]
                } else {
                    (y..y + rows)
                        .map(|line| {
                            let start = line * row + x;
                            (start, &pixels[start..start + columns])
                        })
                        .collect()
                };
                segments
            }))?;
        let writer = &mut self.transport.writer;
        wl_surface::Request::SetBufferScale {
            scale: configuration.scale,
        }
        .encode(window.0, writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Attach {
            buffer: Some(id),
            x: 0,
            y: 0,
        }
        .encode(window.0, writer)
        .map_err(io::Error::other)?;
        for rect in &damage {
            wl_surface::Request::DamageBuffer {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            }
            .encode(window.0, writer)
            .map_err(io::Error::other)?;
        }
        wl_surface::Request::Frame { callback }
            .encode(window.0, writer)
            .map_err(io::Error::other)?;
        wl_surface::Request::Commit
            .encode(window.0, writer)
            .map_err(io::Error::other)?;
        // 4. flush 前冻结 busy 与历史，错误后关闭连接，不能重写可能已交付的内存。
        let state = self.windows.get_mut(&window).expect("validated window");
        state.frame = Some(callback);
        state.history.record(key, pixels, damage);
        let buffer = self.buffers.get_mut(&id).expect("allocated buffer");
        buffer.busy = true;
        buffer.commit = state.history.commits;
        self.transport.flush()?;
        self.windows
            .get_mut(&window)
            .expect("committed window")
            .mapped = true;
        let children: Vec<_> = self
            .pending_parents
            .iter()
            .filter_map(|(child, parent)| (*parent == window).then_some(*child))
            .collect();
        for child in children {
            self.set_parent(child, window)?;
        }
        Ok(())
    }

    fn allocate_buffer(
        &mut self,
        window: WindowId,
        size: (i32, i32),
        stride: i32,
        bytes: i32,
    ) -> io::Result<ObjectId> {
        let memory = MemFd::create("classic-ui", bytes as usize)?;
        let pool = self.transport.allocate(Interface::WlShmPool)?;
        let id = self.transport.allocate(Interface::WlBuffer)?;
        let mut request = Writer::new();
        request.begin(self.shm, 0);
        request.object(Some(pool));
        request.int(bytes);
        request.end().map_err(io::Error::other)?;
        self.transport.send_fd(request.bytes(), memory.as_fd())?;
        wl_shm_pool::Request::CreateBuffer {
            id,
            offset: 0,
            width: size.0,
            height: size.1,
            stride,
            format: wl_shm::Format::Argb8888,
        }
        .encode(pool, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        wl_shm_pool::Request::Destroy
            .encode(pool, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        self.buffers.insert(
            id,
            Buffer {
                window,
                size,
                busy: false,
                commit: 0,
                memory,
            },
        );
        Ok(id)
    }

    pub(super) fn release_buffer(&mut self, object: ObjectId) -> io::Result<()> {
        let Some(buffer) = self.buffers.get_mut(&object) else {
            return Ok(());
        };
        let window = buffer.window;
        if self.windows.contains_key(&window) {
            buffer.busy = false;
            self.events.push_back(super::Event {
                window,
                kind: super::EventKind::BufferAvailable,
            });
        } else {
            self.buffers.remove(&object);
            wl_buffer::Request::Destroy
                .encode(object, &mut self.transport.writer)
                .map_err(io::Error::other)?;
        }
        Ok(())
    }

    pub(super) fn retire_idle_buffers(&mut self, window: WindowId) -> io::Result<()> {
        let ids: Vec<_> = self
            .buffers
            .iter()
            .filter_map(|(id, buffer)| (buffer.window == window && !buffer.busy).then_some(*id))
            .collect();
        for id in ids {
            self.buffers.remove(&id);
            wl_buffer::Request::Destroy
                .encode(id, &mut self.transport.writer)
                .map_err(io::Error::other)?;
        }
        Ok(())
    }
}

/// 单次提交最多声明的损伤矩形数；超过后合并成外接矩形，只会多传、不会漏传。
const DAMAGE_LIMIT: usize = 16;
/// 相隔不超过这么多未变行的变化行并入同一条带，避免逐行文字产生大量细碎矩形。
const DAMAGE_GAP: usize = 8;

/// 逐行比较两帧画面，返回变化区域：相邻变化行合并成条带，列取条带内各行变化范围的并集。
fn changed(previous: &[u32], next: &[u32], width: usize) -> Vec<Rect> {
    let mut bands: Vec<(usize, usize, usize, usize)> = Vec::new();
    for (row, (before, after)) in previous
        .chunks_exact(width)
        .zip(next.chunks_exact(width))
        .enumerate()
    {
        if before == after {
            continue;
        }
        let differs = |(a, b): (&u32, &u32)| a != b;
        let left = before.iter().zip(after).position(differs).unwrap_or(0);
        let right = width
            - before
                .iter()
                .rev()
                .zip(after.iter().rev())
                .position(differs)
                .unwrap_or(0);
        match bands.last_mut() {
            Some((_, bottom, l, r)) if row - *bottom <= DAMAGE_GAP => {
                (*bottom, *l, *r) = (row + 1, (*l).min(left), (*r).max(right));
            }
            _ => bands.push((row, row + 1, left, right)),
        }
    }
    if bands.len() > DAMAGE_LIMIT {
        let merged = bands.iter().fold(bands[0], |(top, bottom, l, r), band| {
            (
                top.min(band.0),
                bottom.max(band.1),
                l.min(band.2),
                r.max(band.3),
            )
        });
        bands = vec![merged];
    }
    bands
        .into_iter()
        .map(|(top, bottom, left, right)| {
            Rect::new(
                left as i32,
                top as i32,
                (right - left) as i32,
                (bottom - top) as i32,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{History, changed};
    use canvas::Rect;

    /// 模拟一块缓冲：按矩形从当前画面复制，与 `present` 写共享内存的方式相同。
    fn write(buffer: &mut [u32], pixels: &[u32], rects: &[Rect], width: usize) {
        for rect in rects {
            for row in rect.y as usize..rect.bottom() as usize {
                let start = row * width + rect.x as usize;
                let end = start + rect.width as usize;
                buffer[start..end].copy_from_slice(&pixels[start..end]);
            }
        }
    }

    #[test]
    fn a_lagging_buffer_catches_up_on_every_missed_commit() {
        let key = (4, 3, 1);
        let mut history = History::default();
        let first = [1; 12];
        assert_eq!(history.damage(key, &first), [Rect::new(0, 0, 4, 3)]);
        history.record(key, &first, vec![Rect::new(0, 0, 4, 3)]);
        let mut lagging = first;
        // 第二帧写进另一块缓冲，`lagging` 停在第 1 次提交。
        let mut second = first;
        second[0] = 2;
        let damage = history.damage(key, &second);
        assert_eq!(damage, [Rect::new(0, 0, 1, 1)]);
        history.record(key, &second, damage);
        let mut third = second;
        third[11] = 3;
        let damage = history.damage(key, &third);
        assert_eq!(damage, [Rect::new(3, 2, 1, 1)]);
        let missed: Vec<Rect> = history
            .since(1, key)
            .unwrap()
            .chain(&damage)
            .copied()
            .collect();
        write(&mut lagging, &third, &missed, 4);
        assert_eq!(
            lagging, third,
            "the second frame's damage is replayed into the lagging buffer"
        );
        history.record(key, &third, damage);
        assert!(history.damage(key, &third).is_empty());
        assert!(
            history.since(0, key).is_none(),
            "a never-written buffer is written whole"
        );
        assert!(
            history.since(3, (8, 3, 1)).is_none(),
            "a size change invalidates the history"
        );
        for _ in 0..super::WINDOW_BUFFER_LIMIT {
            history.record(key, &third, Vec::new());
        }
        assert!(
            history.since(1, key).is_none(),
            "lag beyond the history is written whole"
        );
    }

    #[test]
    fn row_comparison_merges_nearby_rows_into_column_bounded_bands() {
        let width = 8;
        let before = vec![0; width * 30];
        let mut after = before.clone();
        after[2 * width + 3] = 1;
        after[5 * width + 6] = 1;
        after[25 * width] = 1;
        assert_eq!(
            changed(&before, &after, width),
            [Rect::new(3, 2, 4, 4), Rect::new(0, 25, 1, 1)]
        );
        assert!(changed(&before, &before, width).is_empty());
    }
}
