//! wl_surface 提交事务：读取缓冲一次，冻结元数据，同步子 surface 等父提交后原子发布。
use super::{
    client::{Client, Data, Entry, Fail},
    surface::{Committed, Content},
    viewport::{GeometryError, ViewportState},
};
use std::{collections::BTreeMap, rc::Rc};
use wayland_wire::{
    protocol::{wl_buffer, wl_shm, wl_surface},
    wire::ObjectId,
};

impl Client {
    pub(super) fn stage_surface_frame(&mut self, object: ObjectId) {
        if let Ok(surface) = self.surface_mut(object) {
            surface
                .in_flight_callbacks
                .append(&mut surface.queued_callbacks);
        }
    }

    pub(super) fn commit(&mut self, object: ObjectId) -> Result<(), Fail> {
        self.xdg_check_commit(object)?;
        self.layer_check_commit(object)?;
        let surface = self.surface_mut(object)?;
        // 多次同步提交合并到同一缓存；共享像素不可变，不复制当前正在显示的像素。
        let mut commit = surface.cached.take().unwrap_or_else(|| Committed {
            state: surface.current.clone(),
            unmapping: false,
            callbacks: Vec::new(),
            positions: BTreeMap::new(),
        });
        let pending = std::mem::take(&mut surface.pending);
        if let Some((x, y)) = pending.offset
            && !surface.subsurface_role
        {
            commit.state.offset = commit
                .state
                .offset
                .0
                .checked_add(i64::from(x))
                .zip(commit.state.offset.1.checked_add(i64::from(y)))
                .ok_or_else(|| {
                    Fail::new(
                        object,
                        wl_surface::Error::InvalidOffset as u32,
                        "surface coordinate range exhausted",
                    )
                })?;
        }
        if let Some(scale) = pending.scale {
            commit.state.scale = scale;
        }
        if let Some(transform) = pending.transform {
            commit.state.transform = transform;
        }
        if let Some(region) = pending.input_region {
            commit.state.input_region = region;
        }
        if let Some(order) = pending.order {
            commit.state.order = order;
        }
        if let Some(viewport) = surface.viewport.pending.take() {
            commit.state.viewport = viewport;
        }
        commit.callbacks.extend(pending.frame_callbacks);
        match pending.buffer {
            None => {}
            Some(None) => {
                commit.state.content = None;
                commit.unmapping = true;
            }
            Some(Some(attachment)) => {
                commit.unmapping = false;
                let spec = &attachment.buffer;
                let pixels = spec.read_pixels().map_err(|error| match error {
                    super::shm::ReadError::Truncated => Fail::new(
                        attachment.object,
                        wl_shm::Error::InvalidFd as u32,
                        "buffer extends past the end of its file",
                    ),
                    super::shm::ReadError::NoMemory => Fail::new(
                        ObjectId::DISPLAY,
                        wayland_wire::protocol::wl_display::Error::NoMemory as u32,
                        "cannot allocate shared memory snapshot",
                    ),
                })?;
                commit.state.content = Some(Content::new(spec.width, spec.height, pixels));
                // 复制完成后即可 release；但已销毁/复用的对象编号不能收到旧缓冲的事件。
                if matches!(self.objects.get(&attachment.object.get()), Some(Entry { data: Data::Buffer(live), .. }) if Rc::ptr_eq(live, spec))
                {
                    wl_buffer::Event::Release.encode(attachment.object, &mut self.writer)?;
                }
            }
        }
        let viewport = self.surface_mut(object)?.viewport.object.unwrap_or(object);
        ViewportState::validate_size(commit.state.viewport).map_err(|code| {
            Fail::new(viewport, code, "fractional source size needs a destination")
        })?;
        if let Some(content) = &mut commit.state.content {
            let (size, source) = ViewportState::mapping(
                commit.state.viewport,
                content,
                commit.state.scale,
                commit.state.transform,
            )
            .map_err(|error| match error {
                GeometryError::BufferSize => Fail::new(
                    object,
                    wl_surface::Error::InvalidSize as u32,
                    "buffer dimensions are not a multiple of buffer scale",
                ),
                GeometryError::Viewport(code) => {
                    Fail::new(viewport, code, "invalid viewport for the attached buffer")
                }
            })?;
            content.size = size;
            content.source = source;
            content.offset = commit.state.offset;
        }
        commit.positions.extend(self.take_child_positions(object));
        if self.effectively_synchronized(object) {
            self.surface_mut(object)?.cached = Some(commit);
        } else {
            self.apply_surface_commit(object, commit)?;
            self.apply_synchronized_children(object)?;
        }
        Ok(())
    }

    pub(super) fn apply_surface_commit(
        &mut self,
        object: ObjectId,
        commit: Committed,
    ) -> Result<(), Fail> {
        let layer_before = self.layer_state(object);
        let surface = self.surface_mut(object)?;
        let mapping = commit.state.content.is_some();
        surface.current = commit.state;
        surface.queued_callbacks.extend(commit.callbacks);
        let first = mapping && !std::mem::replace(&mut surface.entered, true);
        for (child, position) in commit.positions {
            if let Ok(child) = self.surface_mut(child)
                && let Some(sub) = &mut child.subsurface
            {
                if sub.parent == Some(object) {
                    sub.position = position;
                }
            }
        }
        self.commits += 1;
        if first {
            self.enter_outputs(object)?;
        }
        if commit.unmapping {
            self.xdg_unmap(object);
            self.layer_unmap(object);
        } else {
            self.layer_after_commit(object, layer_before)?;
        }
        self.xdg_after_commit(object)
    }
}
