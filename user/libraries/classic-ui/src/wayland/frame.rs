//! wl_shm 缓冲池：Display 独占内存与 busy 状态，release 之前绝不重写。

use super::{Configuration, Display, WindowId};
use canvas::Rect;
use linux_uapi::shared_memory::{MemFd, SharedMapping};
use std::{collections::VecDeque, io, os::fd::AsFd};
use wayland_wire::{
    protocol::{
        Interface, wl_buffer, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
        wp_viewport, wp_viewporter,
    },
    wire::{ObjectId, Writer},
};

/// 三个在途缓冲覆盖生产者/消费者交错，同时限制慢消费者占用的内存。
const WINDOW_BUFFER_LIMIT: usize = 3;
/// 交互缩放时每个槽最多预留 256 KiB；避免相邻尺寸重复创建和传递 memfd。
const RESIZE_RESERVE: usize = 256 * 1024;

pub(super) struct Buffer {
    pub window: WindowId,
    pub size: (i32, i32),
    pub busy: bool,
    /// 内存里是第几次提交的完整画面；0 表示内容未确认，复用时必须整幅写。
    commit: u64,
    memory: MemFd,
    mapping: SharedMapping,
    pool: ObjectId,
}

impl Buffer {
    fn pixels(&self, count: usize) -> &[u32] {
        assert!(count <= self.mapping.len() / size_of::<u32>());
        // SAFETY: mmap 按页对齐，memfd 封印尺寸。客户端是唯一写入者，
        // 比较阶段不写缓冲且不跨提交保留借用；合成器只读取 wl_shm。
        unsafe { std::slice::from_raw_parts(self.mapping.as_non_null().cast().as_ptr(), count) }
    }

    fn pixels_mut(&mut self, count: usize) -> &mut [u32] {
        assert!(!self.busy && count <= self.mapping.len() / size_of::<u32>());
        // SAFETY: 只有本 Display 写入已封印尺寸、完成预分配的文件。缓冲已经 release，
        // 独占借用期间暂时移出缓冲表，不能同时被损伤比较或提交路径引用。
        unsafe { std::slice::from_raw_parts_mut(self.mapping.as_non_null().cast().as_ptr(), count) }
    }
}

/// 一个窗口最近几次提交的损伤：复用的缓冲落后几帧，就补写这几帧的损伤。
#[derive(Default)]
pub(super) struct History {
    commits: u64,
    /// 最近 [`WINDOW_BUFFER_LIMIT`] 次提交的损伤，最新在后；缓冲最多落后这么多帧。
    damage: VecDeque<Vec<Rect>>,
    /// 历史对应的缓冲像素尺寸与 scale，也是上一次提交的内容规格；变化时历史作废。
    key: Option<(i32, i32, i32)>,
    /// 上一次有像素提交的缓冲。它已保留完整画面，供下一次计算损伤。
    latest: Option<ObjectId>,
}

impl History {
    /// 取消映射后丢弃比较基线。序号继续递增，避免尚未 release 的旧缓冲冒充新提交。
    pub(super) fn invalidate(&mut self) {
        self.key = None;
        self.latest = None;
        self.damage.clear();
    }

    fn record(&mut self, key: (i32, i32, i32), buffer: ObjectId, damage: Vec<Rect>) {
        if self.key != Some(key) {
            self.damage.clear();
            self.key = Some(key);
        }
        self.latest = Some(buffer);
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
        self.present_frame(window, Some(pixels), false, |_, _, _, _| Ok(Vec::new()))
    }

    /// 在已经释放的 Wayland 缓冲中绘制画面，然后提交实际损伤。
    ///
    /// # 参数
    ///
    /// `paint` 接收可写像素、当前配置、内容是否为上一帧，以及只读 Display。
    /// 内容不保留时必须绘制完整画面；否则可局部更新，返回物理像素坐标的损伤矩形。
    ///
    /// # 错误
    ///
    /// 分配、绘制和协议错误原样返回。绘制失败会作废缓冲内容，之后不能把半幅画面用作损伤比较基线。
    pub fn paint_frame(
        &mut self,
        window: WindowId,
        paint: impl FnOnce(&mut [u32], Configuration, bool, &Self) -> io::Result<Vec<Rect>>,
    ) -> io::Result<()> {
        self.present_frame(window, None, false, paint)
    }

    /// 用一个预乘 ARGB `pixel` 覆盖 `window` 的当前逻辑尺寸。
    ///
    /// # Errors
    /// 需要标准 wp_viewporter；其余配置、在途缓冲和提交错误同 [`Self::present`]。
    pub fn present_solid(&mut self, window: WindowId, pixel: u32) -> io::Result<()> {
        self.present_frame(window, Some(&[pixel]), true, |_, _, _, _| Ok(Vec::new()))
    }

    /// 将已证明完全不透明的 surface 逻辑区域加入下一次 commit；None 清除提示。
    ///
    /// # Parameters
    /// `region` 使用 surface 逻辑坐标，调用者必须保证其中每个像素不透明，不能把透明控件层声明为整窗。
    /// # Errors
    /// 窗口不存在或协议编码失败；提示与 buffer 在下一次 commit 原子生效。
    pub fn set_opaque_region(&mut self, window: WindowId, region: Option<Rect>) -> io::Result<()> {
        if !self.windows.contains_key(&window) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let object = region
            .map(|_| self.transport.allocate(Interface::WlRegion))
            .transpose()?;
        let writer = &mut self.transport.writer;
        if let Some((object, region)) = object.zip(region) {
            wl_compositor::Request::CreateRegion { id: object }
                .encode(self.compositor, writer)
                .map_err(io::Error::other)?;
            wl_region::Request::Add {
                x: region.x,
                y: region.y,
                width: region.width,
                height: region.height,
            }
            .encode(object, writer)
            .map_err(io::Error::other)?;
        }
        wl_surface::Request::SetOpaqueRegion { region: object }
            .encode(window.0, writer)
            .map_err(io::Error::other)?;
        if let Some(object) = object {
            wl_region::Request::Destroy
                .encode(object, writer)
                .map_err(io::Error::other)?;
        }
        Ok(())
    }

    fn present_frame(
        &mut self,
        window: WindowId,
        pixels: Option<&[u32]>,
        solid: bool,
        paint: impl FnOnce(&mut [u32], Configuration, bool, &Self) -> io::Result<Vec<Rect>>,
    ) -> io::Result<()> {
        let state = self
            .windows
            .get(&window)
            .filter(|state| state.configured)
            .ok_or_else(|| io::Error::other("window has not been configured"))?;
        if state.frame.is_some() || !self.buffer_available(window) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let configuration = state.configuration;
        let (width, height, scale) = if solid {
            (1, 1, 1)
        } else {
            let (width, height) = configuration.pixel_size()?;
            (width, height, configuration.scale)
        };
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| io::Error::other("shm stride overflow"))?;
        let bytes = stride
            .checked_mul(height)
            .ok_or_else(|| io::Error::other("shm extent overflow"))?;
        if pixels.is_some_and(|pixels| pixels.len() != bytes as usize / 4) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pixel count does not match window configuration",
            ));
        }
        let key = (width, height, scale);
        let bounds = Rect::new(0, 0, width, height);
        let mapped = state.mapped && state.history.key == Some(key);
        let previous = state.history.latest.and_then(|id| self.buffers.get(&id));
        let mut damage = match (previous, pixels) {
            (Some(buffer), Some(pixels))
                if mapped
                    && buffer.window == window
                    && buffer.size == (width, height)
                    && buffer.commit == state.history.commits =>
            {
                changed(buffer.pixels(pixels.len()), pixels, width as usize)
            }
            _ => vec![bounds],
        };
        self.set_viewport_destination(
            window,
            solid.then_some((configuration.width, configuration.height)),
        )?;
        // 1. 缩放时保留能重建几何的空闲池；结束缩放时，即使像素不变也退休其他尺寸。
        let retired: Vec<_> = self
            .buffers
            .iter()
            .filter_map(|(id, buffer)| {
                let fits = configuration.resizing
                    && buffer
                        .memory
                        .len()
                        .checked_sub(bytes as usize)
                        .is_some_and(|slack| slack < RESIZE_RESERVE);
                (buffer.window == window && !buffer.busy && buffer.size != (width, height) && !fits)
                    .then_some(*id)
            })
            .collect();
        for id in retired {
            self.destroy_buffer(id)?;
        }
        // 2. 画面没变且已按同一规格映射：只请求 frame 并 commit。configure 的 ack 要随 commit 生效，
        //    不能因为像素相同就省掉 commit；也不 attach，免得占用缓冲和触发合成器读取。
        if damage.is_empty() && mapped {
            let callback = self.transport.allocate(Interface::WlCallback)?;
            return self.commit_callback(window, callback);
        }
        // 3. 只在 release 后重新绑定原来的池；新几何不继承旧画面的损伤历史。
        let reusable = self
            .buffers
            .iter()
            .filter(|(_, buffer)| buffer.window == window && !buffer.busy)
            .min_by_key(|(_, buffer)| buffer.size != (width, height))
            .map(|(id, _)| *id);
        let id = match reusable {
            Some(id) if self.buffers[&id].size == (width, height) => id,
            Some(id) => {
                let buffer = self.buffers.remove(&id).expect("idle buffer");
                wl_buffer::Request::Destroy
                    .encode(id, &mut self.transport.writer)
                    .map_err(io::Error::other)?;
                self.create_buffer(
                    Buffer {
                        size: (width, height),
                        commit: 0,
                        ..buffer
                    },
                    stride,
                )?
            }
            None => {
                let capacity = if configuration.resizing {
                    (bytes as usize)
                        .next_multiple_of(RESIZE_RESERVE)
                        .min(i32::MAX as usize)
                } else {
                    bytes as usize
                };
                self.allocate_buffer(window, (width, height), stride, capacity)?
            }
        };
        if pixels.is_none() {
            let mut buffer = self.buffers.remove(&id).expect("allocated drawing buffer");
            let held = buffer.commit;
            let preserved = mapped && held == self.windows[&window].history.commits;
            let result = (|| {
                if buffer.commit == 0 {
                    buffer.memory.preallocate()?;
                }
                buffer.commit = 0;
                paint(
                    buffer.pixels_mut(bytes as usize / 4),
                    configuration,
                    preserved,
                    self,
                )
            })();
            self.buffers.insert(id, buffer);
            damage = result?;
            if !preserved {
                damage = vec![bounds];
            }
            for rect in &mut damage {
                *rect = rect.intersect(bounds);
            }
            damage.retain(|rect| !rect.is_empty());
            if damage.is_empty() && mapped {
                self.buffers
                    .get_mut(&id)
                    .expect("drawing buffer returned")
                    .commit = held;
                let callback = self.transport.allocate(Interface::WlCallback)?;
                return self.commit_callback(window, callback);
            }
        }
        // 4. 复用的缓冲停在它上次提交的画面：补写之后各帧与本帧的损伤，新缓冲整幅写。
        if let Some(pixels) = pixels {
            let history = &self.windows[&window].history;
            let buffer = self.buffers.get_mut(&id).expect("allocated buffer");
            let lagging = history.since(buffer.commit, key);
            // 写入或请求编码中途失败时，这块缓冲不再能作为上一帧或损伤补写的基线。
            buffer.commit = 0;
            let current: &[Rect] = if lagging.is_some() {
                &damage
            } else {
                std::slice::from_ref(&bounds)
            };
            let row = width as usize;
            let segments = lagging
                .into_iter()
                .flatten()
                .chain(current)
                .flat_map(|rect| {
                    let (x, y) = (rect.x as usize, rect.y as usize);
                    let (columns, rows) = (rect.width as usize, rect.height as usize);
                    // 宽矩形连同行间像素一次写入（写的是当前画面，多写也正确）；窄矩形逐行写，避免整段搬运。
                    let lines_per_write = if columns * 2 >= row { rows } else { 1 };
                    (y..y + rows).step_by(lines_per_write).map(move |line| {
                        let start = line * row + x;
                        let end = start + (lines_per_write - 1) * row + columns;
                        (start, &pixels[start..end])
                    })
                });
            buffer.memory.write_segments(segments)?;
        }
        // 绘制/分配/写入成功后才登记 callback；这些准备失败时客户端不能遗留永远没有 done/delete_id 的对象。
        let callback = self.transport.allocate(Interface::WlCallback)?;
        let writer = &mut self.transport.writer;
        wl_surface::Request::SetBufferScale { scale }
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
        // 5. flush 前冻结 busy 与历史，错误后关闭连接，不能重写可能已交付的内存。
        let state = self.windows.get_mut(&window).expect("validated window");
        state.frame = Some(callback);
        state.history.record(key, id, damage);
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

    fn commit_callback(&mut self, window: WindowId, callback: ObjectId) -> io::Result<()> {
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
        self.transport.flush()
    }

    pub(super) fn set_viewport_destination(
        &mut self,
        window: WindowId,
        destination: Option<(i32, i32)>,
    ) -> io::Result<()> {
        let state = self.windows.get_mut(&window).expect("validated window");
        if let Some((width, height)) = destination {
            let viewport = match state.viewport {
                Some(viewport) => viewport,
                None => {
                    let manager = self.viewporter.ok_or(io::ErrorKind::Unsupported)?;
                    let viewport = self.transport.allocate(Interface::WpViewport)?;
                    wp_viewporter::Request::GetViewport {
                        id: viewport,
                        surface: window.0,
                    }
                    .encode(manager, &mut self.transport.writer)
                    .map_err(io::Error::other)?;
                    state.viewport = Some(viewport);
                    viewport
                }
            };
            wp_viewport::Request::SetDestination { width, height }
                .encode(viewport, &mut self.transport.writer)
                .map_err(io::Error::other)?;
        } else if let Some(viewport) = state.viewport.take() {
            wp_viewport::Request::Destroy
                .encode(viewport, &mut self.transport.writer)
                .map_err(io::Error::other)?;
        }
        Ok(())
    }

    fn allocate_buffer(
        &mut self,
        window: WindowId,
        size: (i32, i32),
        stride: i32,
        capacity: usize,
    ) -> io::Result<ObjectId> {
        let memory = MemFd::create("classic-ui", capacity)?;
        let mapping = SharedMapping::map(memory.as_fd(), capacity)?;
        let pool = self.create_shm_pool(&memory, capacity)?;
        self.create_buffer(
            Buffer {
                window,
                size,
                busy: false,
                commit: 0,
                memory,
                mapping,
                pool,
            },
            stride,
        )
    }

    /// 两类 SHM 存储共用同一 FD publication；fd 参数只走 SCM_RIGHTS，不写进 Wayland 消息字节。
    pub(super) fn create_shm_pool(
        &mut self,
        memory: &MemFd,
        length: usize,
    ) -> io::Result<ObjectId> {
        let length = i32::try_from(length).map_err(|_| io::ErrorKind::InvalidInput)?;
        let pool = self.transport.allocate(Interface::WlShmPool)?;
        let mut request = Writer::new();
        request.begin(self.shm, 0);
        request.object(Some(pool));
        request.int(length);
        request.end().map_err(io::Error::other)?;
        self.transport.send_fd(request.bytes(), memory.as_fd())?;
        Ok(pool)
    }

    fn create_buffer(&mut self, buffer: Buffer, stride: i32) -> io::Result<ObjectId> {
        let id = self.transport.allocate(Interface::WlBuffer)?;
        wl_shm_pool::Request::CreateBuffer {
            id,
            offset: 0,
            width: buffer.size.0,
            height: buffer.size.1,
            stride,
            format: wl_shm::Format::Argb8888,
        }
        .encode(buffer.pool, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        self.buffers.insert(id, buffer);
        Ok(id)
    }

    fn destroy_buffer(&mut self, id: ObjectId) -> io::Result<()> {
        let buffer = self.buffers.remove(&id).expect("known buffer");
        wl_buffer::Request::Destroy
            .encode(id, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        wl_shm_pool::Request::Destroy
            .encode(buffer.pool, &mut self.transport.writer)
            .map_err(io::Error::other)
    }

    pub(super) fn release_buffer(&mut self, object: ObjectId) -> io::Result<()> {
        let Some(buffer) = self.buffers.get_mut(&object) else {
            return Ok(());
        };
        let window = buffer.window;
        let state = self.windows.get(&window);
        let live = state.is_some();
        if state.is_none_or(|state| {
            !state.configuration.resizing
                && state
                    .history
                    .key
                    .is_some_and(|(w, h, _)| buffer.size != (w, h))
        }) {
            self.destroy_buffer(object)?;
        } else {
            buffer.busy = false;
            // 稳定状态每窗口只保留一块空闲缓冲：第二块只在上一块未释放时临时分配，
            // 释放后立即退休，否则最大化窗口会长期多占一整幅画面的内存。
            // 交互缩放期间保留空闲池，避免相邻尺寸反复创建和传递 memfd。
            let resizing = state.is_some_and(|state| state.configuration.resizing);
            let spare = !resizing
                && self
                    .buffers
                    .iter()
                    .any(|(id, other)| *id != object && other.window == window && !other.busy);
            if spare {
                self.destroy_buffer(object)?;
            }
        }
        if live {
            self.events.push_back(super::Event {
                window,
                kind: super::EventKind::BufferAvailable,
            });
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
            self.destroy_buffer(id)?;
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
    use wayland_wire::wire::ObjectId;

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
        let a = ObjectId::new(2).unwrap();
        let b = ObjectId::new(3).unwrap();
        let first = [1; 12];
        history.record(key, a, vec![Rect::new(0, 0, 4, 3)]);
        let mut lagging = first;
        // 第二帧写进另一块缓冲，`lagging` 停在第 1 次提交。
        let mut second = first;
        second[0] = 2;
        let damage = changed(&first, &second, 4);
        assert_eq!(damage, [Rect::new(0, 0, 1, 1)]);
        history.record(key, b, damage);
        assert_eq!(history.latest, Some(b));
        let mut third = second;
        third[11] = 3;
        let damage = changed(&second, &third, 4);
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
        history.record(key, a, damage);
        assert!(
            history.since(0, key).is_none(),
            "a never-written buffer is written whole"
        );
        assert!(
            history.since(3, (8, 3, 1)).is_none(),
            "a size change invalidates the history"
        );
        for _ in 0..super::WINDOW_BUFFER_LIMIT {
            history.record(key, a, Vec::new());
        }
        assert!(
            history.since(1, key).is_none(),
            "lag beyond the history is written whole"
        );
        let old_commit = history.commits;
        history.invalidate();
        assert_eq!(history.latest, None);
        assert!(history.since(old_commit, key).is_none());
        history.record(key, b, vec![Rect::new(0, 0, 4, 3)]);
        assert!(history.commits > old_commit);
        assert_eq!(
            history
                .since(old_commit, key)
                .unwrap()
                .copied()
                .collect::<Vec<_>>(),
            [Rect::new(0, 0, 4, 3)],
            "a buffer left busy across unmap must be refreshed in full"
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
