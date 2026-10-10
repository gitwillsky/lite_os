//! wait/autoreap 共用的唯一 graph retirement；所有 allocation final Drop 移出 IRQ guard。
use super::*;

/// 已摘下的 graph 与 membership 节点；复用现有 storage，不分配 retirement snapshot。
#[derive(Default)]
pub(super) struct RetiredProcesses {
    nodes: FallibleMap<usize, ProcessNode>,
    parents: FallibleMap<usize, ()>,
    creators: FallibleMap<usize, ()>,
    groups: FallibleMap<usize, ()>,
    group_owners: super::group_retirement::RetiredGroups,
}

impl Drop for RetiredProcesses {
    fn drop(&mut self) {
        // 真实 graph association 已摘除；在 owner guard 外通知所有仍存活 PIDFD 的 HUP。
        for (_, node) in &self.nodes {
            if let Some(source) = node.pidfd_source.upgrade() {
                let ProcessState::Exited(status, _) = node.state else {
                    unreachable!()
                };
                source.record_exit(status.wait_status());
                source.notify();
            }
        }
    }
}

impl RetiredProcesses {
    pub(super) fn retire(&mut self, graph: &mut ProcessGraph, pid: usize) {
        let node = graph
            .nodes
            .get(&pid)
            .expect("retired process missing from graph");
        assert!(matches!(node.state, ProcessState::Exited(..)));
        assert!(
            !matches!(
                node.state,
                ProcessState::Exited(_, ExitRetirement::Retiring)
            ),
            "actual Thread retirement must finish before reap"
        );
        assert!(node.children.is_empty() && node.child_waiters.is_empty());
        assert!(node.child_wait_claim.is_none());
        let (parent, creator, group) = (
            node.parent,
            node.parent_thread,
            (node.session, node.process_group),
        );
        // Process node 是 pending consequence 链的唯一链接 owner；删除前摘掉三条冷链，
        // 否则并发 parent exit/child reap 会令锁外 drain 解引用已删除 PID。
        unlink_consequence(graph, pid, Consequence::ParentDeath);
        for index in 0..2 {
            unlink_consequence(graph, pid, Consequence::ExitEffect(index));
        }
        if let Some(parent) = parent {
            let membership = graph
                .nodes
                .get_mut(&parent)
                .expect("retired parent missing")
                .children
                .take_entry(&pid)
                .expect("retired child missing from parent index");
            self.parents.commit_vacant(membership);
        }
        if let Some(creator) = creator
            && let Some(index) = graph.threads.get_mut(&creator)
            && let Some(membership) = index.created_children.take_entry(&pid)
        {
            self.creators.commit_vacant(membership);
        }
        let membership = graph
            .groups
            .get_mut(&group.1)
            .expect("retired group missing")
            .members
            .take_entry(&pid)
            .expect("retired child missing from group index");
        self.groups.commit_vacant(membership);
        super::group_retirement::retire_empty_group(graph, group, &mut self.group_owners);
        self.nodes.commit_vacant(
            graph
                .nodes
                .take_entry(&pid)
                .expect("retired process disappeared"),
        );
    }
}

#[derive(Clone, Copy)]
enum Consequence {
    ParentDeath,
    ExitEffect(usize),
}
impl Consequence {
    fn head(self, graph: &ProcessGraph) -> Option<usize> {
        match self {
            Self::ParentDeath => graph.pdeath_head,
            Self::ExitEffect(index) => graph.exit_effect_heads[index],
        }
    }
    fn next(self, node: &ProcessNode) -> Option<usize> {
        match self {
            Self::ParentDeath => node.pdeath_next,
            Self::ExitEffect(index) => node.exit_effect_next[index],
        }
    }
    fn set_head(self, graph: &mut ProcessGraph, next: Option<usize>) {
        match self {
            Self::ParentDeath => graph.pdeath_head = next,
            Self::ExitEffect(index) => graph.exit_effect_heads[index] = next,
        }
    }
    fn set_next(self, node: &mut ProcessNode, next: Option<usize>) {
        match self {
            Self::ParentDeath => node.pdeath_next = next,
            Self::ExitEffect(index) => node.exit_effect_next[index] = next,
        }
    }
}
fn unlink_consequence(graph: &mut ProcessGraph, pid: usize, consequence: Consequence) {
    let mut previous = None;
    let mut cursor = consequence.head(graph);
    while let Some(current) = cursor {
        let next = consequence.next(
            graph
                .nodes
                .get(&current)
                .expect("queued consequence process missing"),
        );
        if current == pid {
            if let Some(previous) = previous {
                consequence.set_next(
                    graph
                        .nodes
                        .get_mut(&previous)
                        .expect("queued predecessor missing"),
                    next,
                );
            } else {
                consequence.set_head(graph, next);
            }
            return;
        }
        previous = Some(current);
        cursor = next;
    }
}
