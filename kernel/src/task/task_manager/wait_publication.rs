/// child-wait 最终复查对已准备 node/OOM 的可观察优先级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildWaitPublication {
    ConsumeEvent,
    ReturnNoHang,
    Interrupted,
    OutOfMemory,
    Publish,
}

/// @description 决定 graph 最终复查后的 child-wait 动作。
/// @param event_ready staging 期间是否出现可消费 child event。
/// @param nohang caller 是否请求 WNOHANG。
/// @param interrupted 当前是否已有可投递 signal。
/// @param storage_ready 锁外 waiter node staging 是否成功。
/// @return event/nohang/signal 均先于 staging OOM；仅最后一种组合允许 publication。
pub(super) const fn child_wait_publication(
    event_ready: bool,
    nohang: bool,
    interrupted: bool,
    storage_ready: bool,
) -> ChildWaitPublication {
    if event_ready {
        ChildWaitPublication::ConsumeEvent
    } else if nohang {
        ChildWaitPublication::ReturnNoHang
    } else if interrupted {
        ChildWaitPublication::Interrupted
    } else if !storage_ready {
        ChildWaitPublication::OutOfMemory
    } else {
        ChildWaitPublication::Publish
    }
}
