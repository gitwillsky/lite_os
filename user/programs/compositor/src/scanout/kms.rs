//! 单一输出的标准 atomic KMS 属性装配；ID 由设备 schema 查询，不能写死驱动内部编号。
use linux_uapi::drm::{AtomicRequest, DrmDevice, KmsObject, Topology};
use std::io;

pub(super) struct Output {
    plane: u32,
    framebuffer: u32,
    damage: u32,
    request: AtomicRequest,
    /// 下一次呈现相对前台的输出损伤；不是后缓冲年龄修复区。每帧清空并复用容量。
    clips: Vec<u8>,
}
fn properties(device: &DrmDevice, id: u32, kind: KmsObject) -> io::Result<Vec<(String, u32, u64)>> {
    device
        .properties(id, kind)?
        .into_iter()
        .map(|(id, value)| Ok((device.property(id)?.name, id, value)))
        .collect()
}
fn property(properties: &[(String, u32, u64)], name: &str) -> io::Result<u32> {
    properties
        .iter()
        .find(|entry| entry.0 == name)
        .map(|entry| entry.1)
        .ok_or_else(|| io::Error::other(format!("missing KMS property {name}")))
}
impl Output {
    pub(super) fn new(device: &DrmDevice) -> io::Result<Self> {
        device.set_atomic(true)?;
        for plane in device.plane_ids()? {
            if device.plane(plane)?.possible_crtcs & 1 == 0 {
                continue;
            }
            let properties = properties(device, plane, KmsObject::Plane)?;
            if !properties
                .iter()
                .any(|entry| entry.0 == "type" && entry.2 == 1)
            {
                continue;
            }
            return Ok(Self {
                plane,
                framebuffer: property(&properties, "FB_ID")?,
                damage: property(&properties, "FB_DAMAGE_CLIPS")?,
                request: AtomicRequest::default(),
                clips: Vec::new(),
            });
        }
        Err(io::Error::other("no primary KMS plane"))
    }
    pub(super) fn configure(
        &mut self,
        device: &DrmDevice,
        topology: &Topology,
        framebuffer: u32,
    ) -> io::Result<()> {
        self.request.clear();
        let mode = topology.mode.property_blob(device)?;
        for (object, kind, values) in [
            (
                topology.crtc_id,
                KmsObject::Crtc,
                &[("MODE_ID", u64::from(mode.id())), ("ACTIVE", 1)][..],
            ),
            (
                topology.connector_id,
                KmsObject::Connector,
                &[("CRTC_ID", u64::from(topology.crtc_id))][..],
            ),
        ] {
            let properties = properties(device, object, kind)?;
            for &(name, value) in values {
                self.request
                    .set(object, property(&properties, name)?, value)?;
            }
        }
        let properties = properties(device, self.plane, KmsObject::Plane)?;
        for (name, value) in [
            ("FB_ID", u64::from(framebuffer)),
            ("CRTC_ID", u64::from(topology.crtc_id)),
            ("SRC_X", 0),
            ("SRC_Y", 0),
            ("SRC_W", u64::from(topology.mode.width()) << 16),
            ("SRC_H", u64::from(topology.mode.height()) << 16),
            ("CRTC_X", 0),
            ("CRTC_Y", 0),
            ("CRTC_W", u64::from(topology.mode.width())),
            ("CRTC_H", u64::from(topology.mode.height())),
            ("FB_DAMAGE_CLIPS", 0),
        ] {
            self.request
                .set(self.plane, property(&properties, name)?, value)?;
        }
        device.atomic_commit(&self.request, AtomicRequest::ALLOW_MODESET, 0)
    }
    pub(super) fn damage(&mut self, rectangles: &[canvas::Rect]) {
        self.clips.clear();
        for rectangle in rectangles {
            for value in [
                rectangle.x,
                rectangle.y,
                rectangle.right(),
                rectangle.bottom(),
            ] {
                self.clips.extend_from_slice(&value.to_ne_bytes());
            }
        }
        // 非空 blob 内的空矩形准确表达零 damage；blob ID=0 的语义是全幅更新。
        if self.clips.is_empty() {
            self.clips.resize(16, 0);
        }
    }
    pub(super) fn present(
        &mut self,
        device: &DrmDevice,
        framebuffer: u32,
        sequence: u64,
    ) -> io::Result<()> {
        self.request.clear();
        let damage = if self.clips.is_empty() {
            None
        } else {
            Some(device.create_property_blob(&self.clips)?)
        };
        self.request
            .set(self.plane, self.framebuffer, u64::from(framebuffer))?;
        self.request.set(
            self.plane,
            self.damage,
            damage.as_ref().map_or(0, |blob| u64::from(blob.id())),
        )?;
        // pending/active 配置已接管 blob 引用，提交后即可释放用户创建者引用。
        device.atomic_commit(
            &self.request,
            AtomicRequest::NONBLOCK | AtomicRequest::EVENT,
            sequence,
        )
    }
}
