//! 精确区分配置内容和运行状态更新。配置事务仍以重新导出时的代际执行。

use liteos_ui::Background;
use network_config::{ManagementClient, Revision, configuration::RouterConfiguration};
use std::{io, sync::Arc};

/// 属性窗口打开时导出的配置正文，或已由本服务实例确认的提交正文。
/// 多个属性引用只克隆共享所有权，正文随最后一个草稿释放。
#[derive(Clone)]
pub(crate) struct DraftBaseline {
    pub(crate) revision: Revision,
    document: Arc<[u8]>,
}

impl DraftBaseline {
    /// 读取原生配置正文。代际匹配时保留已有投影，避免重复导出和解码。
    /// 错误：管理服务不可用、无权限或配置正文无效。
    pub(crate) fn capture(
        current: Option<Revision>,
    ) -> io::Result<Option<(Self, RouterConfiguration)>> {
        let mut client = ManagementClient::local();
        if let Some(current) = current
            && client.snapshot()?.generation == current
        {
            return Ok(None);
        }
        let (revision, document) = client.export()?;
        let config = RouterConfiguration::decode(&document).map_err(io::Error::other)?;
        Ok(Some((Self::new(revision, document.into()), config)))
    }

    /// 对仍有效的草稿生成候选。返回重新导出时的代际，由提交端执行最终比较。
    /// 错误：基准失效、配置读取或编码失败，或修改违反领域约束。
    pub(crate) fn candidate(
        &self,
        edit: impl FnOnce(&mut RouterConfiguration) -> io::Result<()>,
    ) -> io::Result<(Revision, Arc<[u8]>)> {
        let (revision, document) = ManagementClient::local().export()?;
        self.verify(revision, &document)?;
        let mut config = RouterConfiguration::decode(&document).map_err(io::Error::other)?;
        edit(&mut config)?;
        Ok((revision, config.encode().map_err(io::Error::other)?.into()))
    }

    /// 保留本次导出或已确认提交的规范 JSON 正文。
    pub(crate) fn new(revision: Revision, document: Arc<[u8]>) -> Self {
        Self { revision, document }
    }

    /// 代际变化时必须验证同一实例及完全相同的正文，才能用当前代际提交。
    ///
    /// 错误：配置内容或服务实例已经变化，原草稿须重新打开。
    pub(crate) fn verify(&self, current: Revision, document: &[u8]) -> io::Result<()> {
        if self.revision == current
            || self.revision.same_instance(current) && &*self.document == document
        {
            Ok(())
        } else {
            Err(io::Error::other("配置已变化，请从最新列表重新打开。"))
        }
    }
}

/// 属性窗口持有的配置快照和固定路由域。修改仍由配置服务校验、应用和持久化。
pub(crate) struct EditTransaction {
    baseline: DraftBaseline,
    scope: Option<Arc<str>>,
    background: Background,
}

impl EditTransaction {
    /// 在打开属性时捕获配置基准及路由域。
    pub(crate) fn new(
        baseline: DraftBaseline,
        scope: Option<Arc<str>>,
        background: Background,
    ) -> Self {
        Self {
            baseline,
            scope,
            background,
        }
    }

    /// 生成并应用局部修改，按确认代际更新本窗的草稿基准。
    /// 错误：所选对象已变化、验证失败或事务未获确认。
    pub(crate) async fn save(
        &mut self,
        edit: impl FnOnce(&mut RouterConfiguration, Option<&str>) -> io::Result<()> + Send + 'static,
    ) -> Result<(), super::Error> {
        let scope = self.scope.clone();
        let baseline = self.baseline.clone();
        let (revision, candidate) = self
            .background
            .run(move || baseline.candidate(|config| edit(config, scope.as_deref())))?
            .await?;
        let result = super::apply(&self.background, candidate.clone(), revision).await?;
        if !result.saved {
            return Err(io::Error::other(result.message).into());
        }
        if let Some(revision) = result.saved_revision {
            self.baseline = DraftBaseline::new(revision, candidate);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU128;

    #[test]
    fn only_identical_configuration_in_the_same_instance_accepts_a_new_revision() {
        let initial = Revision::initial(NonZeroU128::MIN);
        let baseline = DraftBaseline::new(initial, Arc::from(&b"persisted"[..]));
        let advanced = initial.checked_next().unwrap();
        let restarted = Revision::initial(NonZeroU128::new(2).unwrap());

        assert!(baseline.verify(advanced, b"persisted").is_ok());
        assert!(baseline.verify(advanced, b"other").is_err());
        assert!(baseline.verify(restarted, b"persisted").is_err());
    }
}
