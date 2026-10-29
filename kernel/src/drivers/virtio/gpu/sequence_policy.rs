/// controlq command 在完整 display transaction 中的确定性阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuntimeStage {
    DisplayInfo,
    UnrefEvicted,
    Create,
    Attach,
    UnrefCursor,
    CreateCursor,
    AttachCursor,
    TransferCursor,
    TransferScanout,
    SetScanout,
    FlushScanout,
    UnrefBoot,
    FlushDamage,
    UnrefReleased,
    DisableScanout,
    UnrefDisabled(u8),
    Virgl,
    VirglSetScanout,
    VirglFlush,
}

/// completion 与 next command 不属于同一合法 GPU protocol chain。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SequenceOrderError;

impl RuntimeStage {
    /// 判断一个 completion 后选择的下一 command stage 是否符合领域顺序。
    pub(super) const fn allows(self, next: Self) -> bool {
        match (self, next) {
            (Self::UnrefEvicted, Self::Create)
            | (Self::Create, Self::Attach)
            | (Self::Attach, Self::TransferScanout)
            | (Self::UnrefCursor, Self::CreateCursor)
            | (Self::CreateCursor, Self::AttachCursor)
            | (Self::AttachCursor, Self::TransferCursor)
            | (Self::TransferScanout, Self::SetScanout)
            | (Self::SetScanout, Self::FlushScanout)
            | (Self::FlushScanout, Self::UnrefBoot)
            | (Self::VirglSetScanout, Self::VirglFlush)
            | (Self::DisableScanout, Self::UnrefDisabled(_)) => true,
            (Self::UnrefDisabled(previous), Self::UnrefDisabled(next)) => next > previous,
            _ => false,
        }
    }

    /// 验证后继 stage；错误必须在编码 request 或摘取 descriptor 前返回。
    pub(super) const fn validate_successor(self, next: Self) -> Result<(), SequenceOrderError> {
        if self.allows(next) {
            Ok(())
        } else {
            Err(SequenceOrderError)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeStage;

    #[test]
    fn scanout_command_order_accepts_only_the_protocol_chain() {
        let stages = [
            RuntimeStage::UnrefEvicted,
            RuntimeStage::Create,
            RuntimeStage::Attach,
            RuntimeStage::TransferScanout,
            RuntimeStage::SetScanout,
            RuntimeStage::FlushScanout,
            RuntimeStage::UnrefBoot,
        ];
        for pair in stages.windows(2) {
            assert!(pair[0].allows(pair[1]), "rejected {pair:?}");
        }
        assert_eq!(
            RuntimeStage::Create.validate_successor(RuntimeStage::SetScanout),
            Err(super::SequenceOrderError)
        );
        assert_eq!(
            RuntimeStage::Attach.validate_successor(RuntimeStage::FlushScanout),
            Err(super::SequenceOrderError)
        );
    }

    #[test]
    fn disable_unref_loop_rejects_cross_operation_stages() {
        assert!(RuntimeStage::DisableScanout.allows(RuntimeStage::UnrefDisabled(0)));
        assert!(RuntimeStage::UnrefDisabled(0).allows(RuntimeStage::UnrefDisabled(1)));
        assert!(
            RuntimeStage::UnrefDisabled(1)
                .validate_successor(RuntimeStage::UnrefDisabled(0))
                .is_err()
        );
        assert!(
            RuntimeStage::UnrefDisabled(0)
                .validate_successor(RuntimeStage::Create)
                .is_err()
        );
        assert!(
            RuntimeStage::FlushDamage
                .validate_successor(RuntimeStage::UnrefReleased)
                .is_err()
        );
    }

    #[test]
    fn cursor_upload_accepts_only_the_standard_2d_chain() {
        let fresh = [
            RuntimeStage::CreateCursor,
            RuntimeStage::AttachCursor,
            RuntimeStage::TransferCursor,
        ];
        for pair in fresh.windows(2) {
            assert!(pair[0].allows(pair[1]), "rejected {pair:?}");
        }
        assert!(RuntimeStage::UnrefCursor.allows(RuntimeStage::CreateCursor));
        assert!(
            RuntimeStage::CreateCursor
                .validate_successor(RuntimeStage::TransferCursor)
                .is_err()
        );
        assert!(
            RuntimeStage::AttachCursor
                .validate_successor(RuntimeStage::SetScanout)
                .is_err()
        );
    }

    #[test]
    fn virgl_scanout_requires_flush_as_the_visibility_edge() {
        assert!(RuntimeStage::VirglSetScanout.allows(RuntimeStage::VirglFlush));
        assert!(
            RuntimeStage::VirglSetScanout
                .validate_successor(RuntimeStage::Virgl)
                .is_err()
        );
        assert!(
            RuntimeStage::VirglFlush
                .validate_successor(RuntimeStage::VirglSetScanout)
                .is_err()
        );
    }
}
