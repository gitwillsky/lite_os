//! 空 PGID 的真实 ownership retirement；不保留永远增长的历史 group index。
use super::*;
use crate::execution::task::pid::RetiredPidRegistrations;

#[derive(Default)]
pub(super) struct RetiredGroups {
    nodes: FallibleMap<usize, ProcessGroupIndex>,
    links: RetiredPidRegistrations,
}

/// 最后 membership 摘除后回收实际 group；live leader 取回预留 node，旧 links 锁外释放。
pub(super) fn retire_empty_group(
    graph: &mut ProcessGraph,
    key: (usize, usize),
    retired: &mut RetiredGroups,
) {
    if graph
        .groups
        .get(&key.1)
        .is_none_or(|group| !group.members.is_empty())
    {
        return;
    }
    // orphan consequence 链只包含已 stage 的实际 group；摘除前保持 head/next 连通。
    let mut previous = None;
    let mut cursor = graph.exit_group_head;
    while let Some(current) = cursor {
        let next = graph
            .groups
            .get(&current)
            .expect("pending group missing")
            .exit_check_next;
        if current == key.1 {
            if let Some(previous) = previous {
                graph
                    .groups
                    .get_mut(&previous)
                    .expect("pending predecessor missing")
                    .exit_check_next = next;
            } else {
                graph.exit_group_head = next;
            }
            break;
        }
        previous = Some(current);
        cursor = next;
    }
    let entry = graph
        .groups
        .take_entry(&key.1)
        .expect("empty group disappeared");
    if let Some(leader) = graph.nodes.get_mut(&key.1) {
        assert!(
            leader.group_slot.is_none(),
            "group leader already owns its node reservation"
        );
        let (slot, _, group) = entry.into_slot();
        leader.group_slot = Some(slot);
        retired.links.retire(group.pgid);
        retired.links.retire(group.sid);
    } else {
        retired.nodes.commit_vacant(entry);
    }
}
