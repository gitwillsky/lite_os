//! 按既有连接身份区间保留策略。连接与索引共用同一次退休。
use super::{Flows, Observation, Tracked};
use crate::inet::firewall::PolicySnapshot;
use fallible_tree::{FallibleMap, NodeSlot};

pub(in crate::inet) type PolicySlot = NodeSlot<u64, PolicySnapshot>;
pub(in crate::inet) type RetainedPolicies = FallibleMap<u64, PolicySnapshot>;

impl Tracked {
    /// 重新打开 TCP 的首包应使用现行规则。
    pub(in crate::inet::forward) fn existing_generation(&self) -> Option<u64> {
        if self.restart {
            return None;
        }
        match self.observation {
            Observation::Data { generation, .. } => generation,
            Observation::Related { generation, .. } => Some(generation),
        }
    }
}

impl Flows {
    /// 一段身份只对应当时生效的规则。没有新连接时无需保留这一代。
    pub(in crate::inet::forward) fn preserve_policy(
        &mut self,
        program: PolicySnapshot,
        slot: PolicySlot,
    ) -> Option<u64> {
        let previous = self
            .policies
            .predecessor(&u64::MAX)
            .map_or(0, |(&end, _)| end);
        self.identities.successor(&previous)?;
        let end = self.generation;
        self.policies.commit_vacant(slot.fill(end, program));
        Some(end)
    }

    /// 返回该连接适用的历史版本。新连接总使用当前规则。
    pub(in crate::inet::forward) fn retained_policy(
        &self,
        generation: u64,
    ) -> Option<&PolicySnapshot> {
        self.policies.ceiling(&generation).map(|(_, policy)| policy)
    }

    pub(in crate::inet::forward) fn forget_policy(&mut self, boundary: u64) {
        self.policies.remove(&boundary);
    }

    pub(in crate::inet::forward) fn take_policies(&mut self) -> RetainedPolicies {
        core::mem::take(&mut self.policies)
    }

    pub(super) fn retire_policy(&mut self, generation: u64) {
        let Some((&end, _)) = self.policies.ceiling(&generation) else {
            return;
        };
        let previous = self
            .policies
            .predecessor(&end)
            .map_or(0, |(&start, _)| start);
        if self
            .identities
            .successor(&previous)
            .is_none_or(|(&next, _)| next > end)
        {
            self.policies.remove(&end);
        }
    }
}
