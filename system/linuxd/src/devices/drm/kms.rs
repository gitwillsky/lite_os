//! 单输出 KMS mode object 及属性 schema；对象编号与动态 framebuffer/blob 使用同一命名空间。
use super::DrmFile;
mod atomic;
mod configuration;
pub(super) use atomic::Changes;
pub(super) use configuration::Configuration;

pub(super) const CRTC_ID: u32 = 1;
pub(super) const ENCODER_ID: u32 = 2;
pub(super) const CONNECTOR_ID: u32 = 3;
pub(super) const PRIMARY_PLANE_ID: u32 = 4;
pub(super) const OBJECT_CRTC: u32 = 0xcccccccc;
pub(super) const OBJECT_CONNECTOR: u32 = 0xc0c0c0c0;
pub(super) const OBJECT_ENCODER: u32 = 0xe0e0e0e0;
pub(super) const OBJECT_PROPERTY: u32 = 0xb0b0b0b0;
pub(super) const OBJECT_FRAMEBUFFER: u32 = 0xfbfbfbfb;
pub(super) const OBJECT_BLOB: u32 = 0xbbbbbbbb;
pub(super) const OBJECT_PLANE: u32 = 0xeeeeeeee;
pub(super) const PRIMARY_PLANE_TYPE: u64 = 1;
pub(super) const PLANE_TYPES: [(u64, &[u8]); 3] =
    [(0, b"Overlay"), (1, b"Primary"), (2, b"Cursor")];

/// 标准属性的静态 identity；不是 host resource/GEM handle，不能与其互换。
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(super) enum Property {
    Framebuffer = 5,
    Crtc,
    SourceX,
    SourceY,
    SourceWidth,
    SourceHeight,
    CrtcX,
    CrtcY,
    CrtcWidth,
    CrtcHeight,
    PlaneType,
    Damage,
    Active,
    Mode,
}
pub(super) const FIRST_DYNAMIC_ID: u32 = Property::Mode as u32 + 1;
const PROPERTIES: [Property; 14] = [
    Property::Framebuffer,
    Property::Crtc,
    Property::SourceX,
    Property::SourceY,
    Property::SourceWidth,
    Property::SourceHeight,
    Property::CrtcX,
    Property::CrtcY,
    Property::CrtcWidth,
    Property::CrtcHeight,
    Property::PlaneType,
    Property::Damage,
    Property::Active,
    Property::Mode,
];
impl Property {
    /// 按 mode object ID 查找 schema；不存在时返回 None，不读取或创建动态对象。
    pub(super) fn find(id: u32) -> Option<Self> {
        PROPERTIES
            .into_iter()
            .find(|property| *property as u32 == id)
    }
    pub(super) const fn name(self) -> &'static [u8] {
        match self {
            Self::Framebuffer => b"FB_ID",
            Self::Crtc => b"CRTC_ID",
            Self::SourceX => b"SRC_X",
            Self::SourceY => b"SRC_Y",
            Self::SourceWidth => b"SRC_W",
            Self::SourceHeight => b"SRC_H",
            Self::CrtcX => b"CRTC_X",
            Self::CrtcY => b"CRTC_Y",
            Self::CrtcWidth => b"CRTC_W",
            Self::CrtcHeight => b"CRTC_H",
            Self::PlaneType => b"type",
            Self::Damage => b"FB_DAMAGE_CLIPS",
            Self::Active => b"ACTIVE",
            Self::Mode => b"MODE_ID",
        }
    }
    /// UAPI 属性类型及只读/atomic 标签；不是运行时能力标志。
    pub(super) const fn flags(self) -> u32 {
        const ATOMIC: u32 = 1 << 31;
        match self {
            Self::PlaneType => 4 | 8, // IMMUTABLE | ENUM
            Self::Framebuffer | Self::Crtc => ATOMIC | (1 << 6),
            Self::CrtcX | Self::CrtcY => ATOMIC | (1 << 7),
            Self::Damage | Self::Mode => ATOMIC | 16,
            _ => ATOMIC | 2,
        }
    }
    /// range/enum/object 的元数据；blob 载荷必须经 GETPROPBLOB 查询，不进入此列表。
    pub(super) const fn values(self) -> &'static [u64] {
        match self {
            Self::PlaneType => &[0, 1, 2],
            Self::Framebuffer => &[OBJECT_FRAMEBUFFER as u64],
            Self::Crtc => &[OBJECT_CRTC as u64],
            Self::CrtcX | Self::CrtcY => &[i32::MIN as i64 as u64, i32::MAX as u64],
            Self::Active => &[0, 1],
            Self::Damage | Self::Mode => &[],
            _ => &[0, u32::MAX as u64],
        }
    }
}

/// 有属性集合的 KMS 对象；运行状态仍由 completion 的已确认 scanout 唯一拥有。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Object {
    Crtc,
    Connector,
    Primary,
}
impl Object {
    pub(super) const fn index(self) -> usize {
        match self {
            Self::Crtc => 0,
            Self::Connector => 1,
            Self::Primary => 2,
        }
    }
    pub(super) const fn properties(self) -> &'static [Property] {
        match self {
            Self::Crtc => &[Property::Active, Property::Mode],
            Self::Connector => &[Property::Crtc],
            Self::Primary => &[
                Property::Framebuffer,
                Property::Crtc,
                Property::SourceX,
                Property::SourceY,
                Property::SourceWidth,
                Property::SourceHeight,
                Property::CrtcX,
                Property::CrtcY,
                Property::CrtcWidth,
                Property::CrtcHeight,
                Property::PlaneType,
                Property::Damage,
            ],
        }
    }
    pub(super) const fn find(id: u32, kind: u32) -> Option<Self> {
        match id {
            CRTC_ID if kind == 0 || kind == OBJECT_CRTC => Some(Self::Crtc),
            CONNECTOR_ID if kind == 0 || kind == OBJECT_CONNECTOR => Some(Self::Connector),
            PRIMARY_PLANE_ID if kind == 0 || kind == OBJECT_PLANE => Some(Self::Primary),
            _ => None,
        }
    }
}
impl DrmFile {
    /// 本 OFD 是否请求看见 primary plane；未请求时只枚举 overlay（当前硬件没有）。
    pub(super) fn universal_planes(&self) -> bool {
        self.state.lock().universal_planes
    }
    /// 调用方已验证标准 0/1 capability 参数；只改变查询可见性，不改变硬件或 master。
    pub(super) fn set_universal_planes(&self, enabled: bool) {
        self.state.lock().universal_planes = enabled;
    }
    pub(super) fn atomic(&self) -> bool {
        self.state.lock().atomic
    }
    pub(super) fn set_atomic(&self, enabled: bool) {
        let mut state = self.state.lock();
        state.atomic = enabled;
        if enabled {
            state.universal_planes = true;
        }
    }
    /// 区分查不到对象与对象没有 property 集合；查询不保活已退休 payload。
    pub(super) fn mode_object_without_properties(&self, id: u32, kind: u32) -> bool {
        if id == ENCODER_ID && (kind == 0 || kind == OBJECT_ENCODER) {
            return true;
        }
        if (kind == 0 || kind == OBJECT_PROPERTY) && Property::find(id).is_some() {
            return true;
        }
        if (kind == 0 || kind == OBJECT_FRAMEBUFFER)
            && self.device.state.lock().framebuffers.contains_key(&id)
        {
            return true;
        }
        (kind == 0 || kind == OBJECT_BLOB) && self.property_blob(id).is_ok()
    }
}

impl super::CompletionState {
    /// 查询/TEST_ONLY 的逻辑状态包括已接受但 fence 未完成的候选；像素/光标仍只用已确认 active。
    pub(super) fn configuration(&self) -> Option<&Configuration> {
        if self.reset_after_owner.is_some() {
            return None;
        }
        match &self.pending {
            Some(super::PendingDisplay {
                operation: super::PendingOperation::Commit { next, .. },
                ..
            }) => next.as_ref(),
            _ => self.active.as_ref(),
        }
    }
}
impl DrmFile {
    /// 用户引用已在 Changes 中固定；候选先声明，使错误路径也在 completion guard 之后析构。
    pub(super) fn atomic_commit(
        self: &alloc::sync::Arc<Self>,
        changes: &Changes,
        test: bool,
        allow_modeset: bool,
        event: Option<u64>,
    ) -> Result<Option<super::DrmSubmission>, super::DrmError> {
        let candidate;
        let mut completion = self.device.completion.lock();
        if !self.is_master() {
            return Err(super::DrmError::Permission);
        }
        if changes.is_empty() {
            return if event.is_some() {
                Err(super::DrmError::Invalid)
            } else {
                Ok(None)
            };
        }
        candidate = changes.apply(
            completion.configuration(),
            self.file_identity,
            allow_modeset,
        )?;
        if test {
            return Ok(None);
        }
        if event.is_none()
            && !candidate.is_scanning()
            && completion.pending.is_none()
            && completion.reset_after_owner.is_none()
            && completion
                .active
                .as_ref()
                .is_none_or(|active| !active.is_scanning())
        {
            // 非扫描状态只改变逻辑属性，不重复发送硬件 disable；旧 blob 必须在 completion 锁外退休。
            let retired = completion.active.replace(candidate);
            drop(completion);
            drop(retired);
            return Ok(None);
        }
        let generation = completion.adapter_generation;
        let event = event.map(|user_data| super::PendingEvent {
            file: alloc::sync::Arc::downgrade(self),
            user_data,
        });
        super::classify_submission(
            self,
            generation,
            self.submit_configuration_locked(&mut completion, &candidate, event),
        )
        .map(Some)
    }
}
