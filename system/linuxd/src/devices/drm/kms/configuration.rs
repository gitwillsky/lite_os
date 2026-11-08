//! 一次不可变 KMS 配置；mode、路由、CRTC 活动与 primary 绑定是不同标准属性，不能用一个 framebuffer ID 代替。
use super::super::{DisplayMode, DrmError, DrmFile, Framebuffer, mode::cvt_mode, property::Blob};
use super::{CRTC_ID, Object, Property};
use alloc::{sync::Arc, vec::Vec};

#[derive(Clone)]
pub(super) struct Mode {
    blob: Arc<Blob>,
    native: DisplayMode,
}
impl Mode {
    pub(super) fn parse(file: &DrmFile, blob: Arc<Blob>) -> Result<Self, DrmError> {
        if blob.bytes.len() != 68 {
            return Err(DrmError::Invalid);
        }
        let width = u32::from(u16::from_ne_bytes(
            blob.bytes[4..6].try_into().expect("mode hdisplay"),
        ));
        let height = u32::from(u16::from_ne_bytes(
            blob.bytes[14..16].try_into().expect("mode vdisplay"),
        ));
        let (native, canonical) = file.requested_mode(width, height)?;
        let word = |offset| {
            u32::from_ne_bytes(
                blob.bytes[offset..offset + 4]
                    .try_into()
                    .expect("mode word"),
            )
        };
        if word(0) > i32::MAX as u32 || word(24) > i32::MAX as u32 {
            return Err(DrmError::OutOfRange);
        }
        // timing 是硬件约束；type/name/vrefresh 是 UAPI 提示，不得要求与首选模式逐字相等。
        let aspect = word(28) & (15 << 19);
        if blob.bytes[..24] != canonical.encode()[..24]
            || word(28) & !(15 << 19) != canonical.flags
            || aspect >> 19 > 4
        {
            return Err(DrmError::Invalid);
        }
        Ok(Self { blob, native })
    }
}

/// 提交候选或已确认配置的唯一载荷；Clone 只复制固定元数据与真实对象引用，不复制像素/模式字节。
#[derive(Clone)]
pub(in crate::devices::drm) struct Configuration {
    pub(super) owner: u64,
    pub(super) mode: Option<Mode>,
    pub(super) active: bool,
    pub(super) connected: bool,
    pub(super) plane_crtc: bool,
    pub(super) framebuffer: Option<Arc<Framebuffer>>,
    /// buffer 坐标，16.16，依次 x/y/width/height。
    pub(super) source: [u32; 4],
    /// 有符号位置与无符号尺寸，输出像素。
    pub(super) target: (i32, i32, u32, u32),
    pub(super) damage: Option<Arc<Blob>>,
}
impl Configuration {
    pub(super) const fn empty(owner: u64) -> Self {
        Self {
            owner,
            mode: None,
            active: false,
            connected: false,
            plane_crtc: false,
            framebuffer: None,
            source: [0; 4],
            target: (0, 0, 0, 0),
            damage: None,
        }
    }
    pub(in crate::devices::drm) fn owner(&self) -> u64 {
        self.owner
    }
    pub(in crate::devices::drm) fn is_scanning(&self) -> bool {
        self.active
            && self.connected
            && self.plane_crtc
            && self.framebuffer.is_some()
            && self.mode.is_some()
    }
    pub(in crate::devices::drm) fn native_mode(&self) -> Option<DisplayMode> {
        self.mode.as_ref().map(|mode| mode.native)
    }
    /// GETCRTC 返回内核归一化的 mode；GETPROPBLOB 仍返回用户原始不可变载荷。
    pub(in crate::devices::drm) fn encoded_mode(&self) -> Option<[u8; 68]> {
        self.mode.as_ref().map(|mode| {
            let timing = cvt_mode(mode.native);
            let mut bytes = timing.encode();
            let total = u64::from(timing.htotal) * u64::from(timing.vtotal);
            let refresh = ((u64::from(timing.clock) * 1000 + total / 2) / total) as u32;
            bytes[24..28].copy_from_slice(&refresh.to_ne_bytes());
            bytes[28..32].copy_from_slice(&mode.blob.bytes[28..32]);
            let kind = u32::from_ne_bytes(mode.blob.bytes[32..36].try_into().expect("mode type"))
                & ((1 << 3) | (1 << 5) | (1 << 6));
            bytes[32..36].copy_from_slice(&kind.to_ne_bytes());
            bytes[36..].fill(0);
            let name = &mode.blob.bytes[36..67];
            let length = name
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(name.len());
            bytes[36..36 + length].copy_from_slice(&name[..length]);
            bytes
        })
    }
    pub(in crate::devices::drm) fn framebuffer(&self) -> Option<&Arc<Framebuffer>> {
        self.framebuffer.as_ref()
    }
    pub(in crate::devices::drm) fn primary_binding(&self) -> (u32, u32) {
        (
            if self.plane_crtc { CRTC_ID } else { 0 },
            self.framebuffer
                .as_ref()
                .map_or(0, |framebuffer| framebuffer.id),
        )
    }
    pub(in crate::devices::drm) fn value(&self, object: Object, property: Property) -> u64 {
        match property {
            Property::Framebuffer => self
                .framebuffer
                .as_ref()
                .map_or(0, |framebuffer| u64::from(framebuffer.id)),
            Property::Crtc => {
                u64::from(match object {
                    Object::Connector => self.connected,
                    _ => self.plane_crtc,
                }) * u64::from(CRTC_ID)
            }
            Property::SourceX => u64::from(self.source[0]),
            Property::SourceY => u64::from(self.source[1]),
            Property::SourceWidth => u64::from(self.source[2]),
            Property::SourceHeight => u64::from(self.source[3]),
            Property::CrtcX => self.target.0 as i64 as u64,
            Property::CrtcY => self.target.1 as i64 as u64,
            Property::CrtcWidth => u64::from(self.target.2),
            Property::CrtcHeight => u64::from(self.target.3),
            Property::PlaneType => super::PRIMARY_PLANE_TYPE,
            Property::Active => u64::from(self.active),
            Property::Mode => self.mode.as_ref().map_or(0, |mode| u64::from(mode.blob.id)),
            Property::Damage => self.damage.as_ref().map_or(0, |blob| u64::from(blob.id)),
        }
    }
    /// 原生 adapter 只有全屏、无缩放 primary；非法组合/超出机制的几何必须在发表前拒绝。
    pub(in crate::devices::drm) fn validate(&self) -> Result<(), DrmError> {
        if self.active && self.mode.is_none()
            || self.plane_crtc != self.framebuffer.is_some()
            || self.connected && self.mode.is_none()
        {
            return Err(DrmError::Invalid);
        }
        if let Some(framebuffer) = &self.framebuffer {
            let native = self.native_mode().ok_or(DrmError::Invalid)?;
            if (framebuffer.width, framebuffer.height, framebuffer.pitch)
                != (native.width, native.height, native.pitch)
                || self.source != [0, 0, native.width << 16, native.height << 16]
                || self.target != (0, 0, native.width, native.height)
            {
                return Err(DrmError::Invalid);
            }
        }
        Ok(())
    }
    /// 标准 damage helper 的裁剪/并集语义；首次呈现、源或 mode 改变时必须全幅。
    /// None 表示所有给定矩形裁剪后为空，可以绑定目标而省略 host 刷新通知。
    pub(in crate::devices::drm) fn flush_damage(
        &self,
        previous: Option<&Self>,
    ) -> Option<super::super::DisplayRect> {
        let mode = self.native_mode()?;
        let full = super::super::DisplayRect {
            x: 0,
            y: 0,
            width: mode.width,
            height: mode.height,
        };
        let unchanged = previous.is_some_and(|previous| {
            previous.is_scanning()
                && previous.native_mode() == Some(mode)
                && previous.source == self.source
                && previous.target == self.target
        });
        if !unchanged {
            return Some(full);
        }
        let Some(damage) = &self.damage else {
            return Some(full);
        };
        let mut merged: Option<(i32, i32, i32, i32)> = None;
        for rectangle in damage.bytes.as_chunks::<16>().0 {
            let word = |offset| {
                i32::from_ne_bytes(
                    rectangle[offset..offset + 4]
                        .try_into()
                        .expect("damage coordinate"),
                )
            };
            let (x1, y1, x2, y2) = (
                word(0).max(0),
                word(4).max(0),
                word(8).min(mode.width as i32),
                word(12).min(mode.height as i32),
            );
            if x2 <= x1 || y2 <= y1 {
                continue;
            }
            merged = Some(merged.map_or((x1, y1, x2, y2), |old| {
                (old.0.min(x1), old.1.min(y1), old.2.max(x2), old.3.max(y2))
            }));
        }
        merged.map(|(x1, y1, x2, y2)| super::super::DisplayRect {
            x: x1 as u32,
            y: y1 as u32,
            width: (x2 - x1) as u32,
            height: (y2 - y1) as u32,
        })
    }
    /// legacy SETCRTC 也建立同一标准 mode blob 与配置；状态引用保活，不添加用户创建者引用。
    pub(in crate::devices::drm) fn legacy(
        file: &DrmFile,
        framebuffer: Arc<Framebuffer>,
        native: DisplayMode,
    ) -> Result<Self, DrmError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(68)
            .map_err(|_| DrmError::OutOfMemory)?;
        bytes.extend_from_slice(&cvt_mode(native).encode());
        let mode = Mode::parse(file, file.state_blob(bytes)?)?;
        let configuration = Self {
            owner: file.file_identity,
            mode: Some(mode),
            active: true,
            connected: true,
            plane_crtc: true,
            framebuffer: Some(framebuffer),
            source: [0, 0, native.width << 16, native.height << 16],
            target: (0, 0, native.width, native.height),
            damage: None,
        };
        configuration.validate()?;
        Ok(configuration)
    }
    /// legacy 翻页只替换真实 framebuffer，模式引用与其标准 identity 不随每帧重建。
    pub(in crate::devices::drm) fn page_flip(
        &self,
        owner: u64,
        framebuffer: Arc<Framebuffer>,
    ) -> Result<Self, DrmError> {
        if !self.is_scanning() {
            return Err(DrmError::Invalid);
        }
        let mut next = self.clone();
        next.owner = owner;
        next.framebuffer = Some(framebuffer);
        next.damage = None;
        next.validate()?;
        Ok(next)
    }
}
