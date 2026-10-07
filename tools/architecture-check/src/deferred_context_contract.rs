use quote::ToTokens;
use syn::{Arm, Expr, ExprCall, ExprMatch, ImplItem, ImplItemFn, Item, ItemFn, Pat, visit::Visit};

use super::SourceFile;

const TRAP_SOURCE: &str = "kernel/src/trap/mod.rs";
const CPU_DEFERRED_SOURCE: &str = "kernel/src/hardware/deferred.rs";
const PROCESS_TABLE_SOURCE: &str = "kernel/src/task/process_table.rs";
const CONTEXT_SWITCH_SOURCE: &str = "kernel/src/task/process_table/context_switch.rs";
const VIRTIO_LOCKS: &[(&str, &str, &str, &str)] = &[
    (
        "kernel/src/devices/virtio/net.rs",
        "VirtIONetworkDevice",
        "queues",
        "Mutex < QueueState >",
    ),
    (
        "kernel/src/devices/virtio/input.rs",
        "VirtIOInputDevice",
        "events",
        "Mutex < EventQueueState >",
    ),
    (
        "kernel/src/devices/virtio/gpu.rs",
        "VirtIOGpuDevice",
        "control",
        "Mutex < ControlQueue >",
    ),
];

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(trap) = sources.iter().find(|source| source.relative == TRAP_SOURCE) else {
        errors.push(format!("{TRAP_SOURCE}: missing deferred execution owner"));
        return;
    };
    let Some(process_table) = sources
        .iter()
        .find(|source| source.relative == PROCESS_TABLE_SOURCE)
    else {
        errors.push(format!(
            "{PROCESS_TABLE_SOURCE}: missing scheduler deferred safe point"
        ));
        return;
    };

    check_software_interrupt_acknowledgement(trap, errors);
    check_unique_ssip_acknowledger(sources, errors);
    check_kernel_trap_does_not_dispatch(trap, errors);
    check_user_return_dispatches(trap, errors);
    check_idle_dispatch_is_irq_closed(process_table, errors);
    check_task_handoff_dispatch_is_irq_closed(sources, errors);
    check_unique_dispatch_callers(sources, errors);
    check_deferred_notification_coalescing(sources, errors);
    check_virtio_irq_and_lock_contract(sources, errors);
    check_kernel_space_lock_track(sources, errors);
    check_driver_io_dispatch_lock_free(sources, errors);
}

const DRIVER_REGISTRY_SOURCE: &str = "kernel/src/devices/drivers/registry.rs";

/// 方法调用名称计数器。
#[derive(Default)]
struct MethodCalls(Vec<String>);

impl<'ast> Visit<'ast> for MethodCalls {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.0.push(call.method.to_string());
        syn::visit::visit_expr_method_call(self, call);
    }
}

/// 每次块设备/RNG/声卡 completion 都经 `dispatch_io_completion_work` 遍历 completion 源：该热路径
/// 只允许无锁读取只追加的 `Once` 槽位，不得取锁或克隆 `Arc`（引用计数原子操作）。
fn check_driver_io_dispatch_lock_free(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(source) = sources
        .iter()
        .find(|source| source.relative == DRIVER_REGISTRY_SOURCE)
    else {
        errors.push(format!("{DRIVER_REGISTRY_SOURCE}: missing driver registry"));
        return;
    };
    let Some(dispatch) = function(source, "dispatch_io_completion_work") else {
        errors.push(format!(
            "{DRIVER_REGISTRY_SOURCE}: missing dispatch_io_completion_work"
        ));
        return;
    };
    let mut calls = MethodCalls::default();
    calls.visit_block(&dispatch.block);
    let forbidden = calls
        .0
        .into_iter()
        .filter(|method| matches!(method.as_str(), "lock" | "clone" | "cloned"))
        .collect::<Vec<_>>();
    if !forbidden.is_empty() {
        errors.push(format!(
            "{DRIVER_REGISTRY_SOURCE}: DRIVER_IO completion dispatch must read sources lock-free; found {forbidden:?}"
        ));
    }
}

fn check_deferred_notification_coalescing(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(source) = sources
        .iter()
        .find(|source| source.relative == CPU_DEFERRED_SOURCE)
    else {
        errors.push(format!(
            "{CPU_DEFERRED_SOURCE}: missing deferred bitmap owner"
        ));
        return;
    };
    let Some(raise) = function(source, "raise") else {
        errors.push(format!("{CPU_DEFERRED_SOURCE}: missing deferred publisher"));
        return;
    };
    let body = raise.block.to_token_stream().to_string();
    let publish = body.find("fetch_or");
    let transition = body.find("if previous == 0");
    let notify = body.find("raise_software");
    if !matches!((publish, transition, notify), (Some(publish), Some(transition), Some(notify)) if publish < transition && transition < notify)
    {
        errors.push(format!(
            "{CPU_DEFERRED_SOURCE}: local deferred notification must be issued only on the bitmap empty-to-nonempty transition"
        ));
    }
}

fn check_unique_ssip_acknowledger(sources: &[SourceFile], errors: &mut Vec<String>) {
    let mut callers = Vec::new();
    for source in sources {
        let mut calls = NamedCallCount::new("clear_software");
        calls.visit_file(&source.syntax);
        callers.extend(core::iter::repeat_n(source.relative.as_str(), calls.count));
    }
    if callers != [TRAP_SOURCE] {
        errors.push(format!(
            "SSIP acknowledgement must remain unique to the trap-owned clear-then-barrier seam; found {callers:?}"
        ));
    }
}

fn check_task_handoff_dispatch_is_irq_closed(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(source) = sources
        .iter()
        .find(|source| source.relative == CONTEXT_SWITCH_SOURCE)
    else {
        errors.push(format!(
            "{CONTEXT_SWITCH_SOURCE}: missing direct handoff owner"
        ));
        return;
    };
    let Some(handoff) = function(source, "schedule_with_task_context") else {
        errors.push(format!("{CONTEXT_SWITCH_SOURCE}: missing task handoff"));
        return;
    };
    let body = handoff.block.to_token_stream().to_string();
    let irq = body.find("LocalIrqGuard :: disable");
    let dispatch = body.find("scheduler_deferred_safe_point");
    let select = body.find("select_task_switch_target");
    if !matches!((irq, dispatch, select), (Some(irq), Some(dispatch), Some(select)) if irq < dispatch && dispatch < select)
    {
        errors.push(format!(
            "{CONTEXT_SWITCH_SOURCE}: direct handoff safe point must run after local IRQ disable and before successor selection"
        ));
    }
}

fn check_unique_dispatch_callers(sources: &[SourceFile], errors: &mut Vec<String>) {
    let mut callers = Vec::new();
    for source in sources {
        let mut calls = NamedCallCount::new("dispatch_pending_deferred_work");
        calls.visit_file(&source.syntax);
        callers.extend(core::iter::repeat_n(source.relative.as_str(), calls.count));
    }
    callers.sort_unstable();
    let mut expected = vec![PROCESS_TABLE_SOURCE, TRAP_SOURCE];
    expected.sort_unstable();
    if callers != expected {
        errors.push(format!(
            "deferred domain dispatch must have exactly the user-return and IRQ-closed idle callers; found {callers:?}"
        ));
    }
}

struct NamedCallCount<'a> {
    name: &'a str,
    count: usize,
}

impl<'a> NamedCallCount<'a> {
    fn new(name: &'a str) -> Self {
        Self { name, count: 0 }
    }
}

impl<'ast> Visit<'ast> for NamedCallCount<'_> {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if matches!(&*call.func, Expr::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == self.name))
        {
            self.count += 1;
        }
        syn::visit::visit_expr_call(self, call);
    }
}

fn check_kernel_space_lock_track(sources: &[SourceFile], errors: &mut Vec<String>) {
    const PATH: &str = "kernel/src/memory/mod.rs";
    let Some(source) = sources.iter().find(|source| source.relative == PATH) else {
        errors.push(format!("{PATH}: missing kernel address-space owner"));
        return;
    };
    let types = source.syntax.items.iter().filter_map(|item| match item {
        Item::Static(item) if item.ident == "KERNEL_SPACE" => {
            Some(item.ty.to_token_stream().to_string())
        }
        _ => None,
    });
    let actual = types.collect::<Vec<_>>();
    if actual != ["Once < Mutex < MemorySet > >"] {
        errors.push(format!(
            "{PATH}: KERNEL_SPACE must remain the sole ordinary task/safe-point lock; found {actual:?}"
        ));
    }
}

fn check_virtio_irq_and_lock_contract(sources: &[SourceFile], errors: &mut Vec<String>) {
    for &(path, device_name, field_name, expected_type) in VIRTIO_LOCKS {
        let Some(source) = sources.iter().find(|source| source.relative == path) else {
            errors.push(format!("{path}: missing VirtIO deferred adapter"));
            continue;
        };
        let lock_types = source.syntax.items.iter().filter_map(|item| match item {
            Item::Struct(item) if item.ident == device_name => item
                .fields
                .iter()
                .find(|field| field.ident.as_ref().is_some_and(|name| name == field_name))
                .map(|field| field.ty.to_token_stream().to_string()),
            _ => None,
        });
        let actual = lock_types.collect::<Vec<_>>();
        if actual != [expected_type] {
            errors.push(format!(
                "{path}: {device_name}.{field_name} must remain the sole ordinary safe-point lock `{expected_type}`; found {actual:?}"
            ));
        }

        let handlers = source.syntax.items.iter().filter_map(|item| match item {
            Item::Impl(implementation)
                if implementation.trait_.as_ref().is_some_and(|(_, path, _)| {
                    path.segments
                        .last()
                        .is_some_and(|segment| segment.ident == "InterruptHandler")
                }) =>
            {
                implementation.items.iter().find_map(|item| match item {
                    ImplItem::Fn(method) if method.sig.ident == "handle_interrupt" => Some(method),
                    _ => None,
                })
            }
            _ => None,
        });
        let handlers = handlers.collect::<Vec<_>>();
        let [handler] = handlers.as_slice() else {
            errors.push(format!(
                "{path}: expected exactly one InterruptHandler::handle_interrupt"
            ));
            continue;
        };
        let mut audit = HardirqAudit::default();
        audit.visit_block(&handler.block);
        // adapter 经 `raise_completion` 发布消费者绑定的 vector 时，其函数体本身必须恰好发布一次
        // 且同样不得进入 queue/page-table state。
        if audit.bound_publications != 0 {
            let helpers = source
                .syntax
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Impl(implementation) => Some(implementation),
                    _ => None,
                })
                .flat_map(|implementation| implementation.items.iter())
                .filter_map(|item| match item {
                    ImplItem::Fn(method) if method.sig.ident == "raise_completion" => Some(method),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let mut helper_audit = HardirqAudit::default();
            if let [helper] = helpers.as_slice() {
                helper_audit.visit_block(&helper.block);
            }
            if helpers.len() != 1
                || helper_audit.deferred_publications != 1
                || helper_audit.bound_publications != 0
                || !helper_audit.forbidden.is_empty()
            {
                errors.push(format!(
                    "{path}: raise_completion must be the sole helper and publish exactly one deferred bit without entering queue/page-table state; publications={}, forbidden={:?}",
                    helper_audit.deferred_publications, helper_audit.forbidden
                ));
            }
        }
        if audit.deferred_publications + audit.bound_publications != 1
            || !audit.forbidden.is_empty()
        {
            errors.push(format!(
                "{path}: VirtIO hardirq must publish exactly one deferred bit and may not enter queue/page-table state; publications={}, forbidden={:?}",
                audit.deferred_publications + audit.bound_publications,
                audit.forbidden
            ));
        }
    }
}

#[derive(Default)]
struct HardirqAudit {
    deferred_publications: usize,
    /// 经 adapter `raise_completion` 发布消费者绑定 vector 的次数。
    bound_publications: usize,
    forbidden: Vec<String>,
}

impl<'ast> Visit<'ast> for HardirqAudit {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let Expr::Path(function) = &*call.func {
            let segments = function
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>();
            let name = segments.last().cloned();
            if matches!(segments.as_slice(), [.., module, name] if module == "deferred" && name == "raise")
            {
                self.deferred_publications += 1;
            }
            if matches!(
                name.as_deref(),
                Some("add_buffer" | "dispatch_pending_deferred_work")
            ) {
                self.forbidden.push(name.unwrap());
            }
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let method = call.method.to_string();
        if method == "raise_completion" {
            self.bound_publications += 1;
        }
        if matches!(
            method.as_str(),
            "lock" | "add_buffer" | "used" | "poll" | "poll_update" | "receive_event"
        ) {
            self.forbidden.push(method);
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if path
            .path
            .segments
            .iter()
            .any(|segment| segment.ident == "KERNEL_SPACE")
        {
            self.forbidden.push("KERNEL_SPACE".to_owned());
        }
        syn::visit::visit_expr_path(self, path);
    }
}

fn function<'a>(source: &'a SourceFile, name: &str) -> Option<&'a ItemFn> {
    source.syntax.items.iter().find_map(|item| match item {
        syn::Item::Fn(function) if function.sig.ident == name => Some(function),
        _ => None,
    })
}

fn direct_call_name(expression: &Expr) -> Option<String> {
    let Expr::Call(call) = expression else {
        return None;
    };
    let Expr::Path(path) = &*call.func else {
        return None;
    };
    Some(
        path.path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::"),
    )
}

fn direct_statement_call(statement: &syn::Stmt) -> Option<String> {
    match statement {
        syn::Stmt::Expr(expression, _) => direct_call_name(expression),
        _ => None,
    }
}

fn software_interrupt_arm(function: &ItemFn) -> Option<&Arm> {
    struct MatchVisitor<'a> {
        arm: Option<&'a Arm>,
    }
    impl<'ast> Visit<'ast> for MatchVisitor<'ast> {
        fn visit_expr_match(&mut self, expression: &'ast ExprMatch) {
            if self.arm.is_none() {
                self.arm = expression.arms.iter().find(|arm| {
                    matches!(&arm.pat, Pat::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == "SoftwareInterrupt"))
                });
            }
            if self.arm.is_none() {
                syn::visit::visit_expr_match(self, expression);
            }
        }
    }
    let mut visitor = MatchVisitor { arm: None };
    visitor.visit_item_fn(function);
    visitor.arm
}

fn check_software_interrupt_acknowledgement(source: &SourceFile, errors: &mut Vec<String>) {
    let Some(handler) = function(source, "handle_supervisor_soft_interrupt") else {
        errors.push(format!(
            "{TRAP_SOURCE}: missing handle_supervisor_soft_interrupt owner seam"
        ));
        return;
    };
    let calls = handler
        .block
        .stmts
        .iter()
        .filter_map(direct_statement_call)
        .collect::<Vec<_>>();
    let expected = [
        "arch::interrupt::clear_software",
        "crate::task::complete_pending_memory_barrier",
    ];
    if calls != expected {
        errors.push(format!(
            "{TRAP_SOURCE}: supervisor software interrupt must first acknowledge SSIP and then complete the synchronous memory barrier, with no deferred domain dispatch; found {calls:?}"
        ));
    }
}

fn check_kernel_trap_does_not_dispatch(source: &SourceFile, errors: &mut Vec<String>) {
    let Some(handler) = function(source, "handle_kernel_trap") else {
        errors.push(format!("{TRAP_SOURCE}: missing handle_kernel_trap"));
        return;
    };
    let Some(arm) = software_interrupt_arm(handler) else {
        errors.push(format!(
            "{TRAP_SOURCE}: kernel trap lost SoftwareInterrupt acknowledgement arm"
        ));
        return;
    };
    let mut calls = CallVisitor::default();
    calls.visit_expr(&arm.body);
    if calls.names != ["handle_supervisor_soft_interrupt"] {
        errors.push(format!(
            "{TRAP_SOURCE}: kernel SoftwareInterrupt may only acknowledge the trap-owned SSIP/barrier seam; arbitrary kernel context must not consume deferred domains, found {:?}",
            calls.names
        ));
    }
}

fn check_user_return_dispatches(source: &SourceFile, errors: &mut Vec<String>) {
    let Some(handler) = function(source, "handle_user_trap") else {
        errors.push(format!("{TRAP_SOURCE}: missing handle_user_trap"));
        return;
    };
    let match_index = handler
        .block
        .stmts
        .iter()
        .position(|statement| matches!(statement, syn::Stmt::Expr(Expr::Match(_), _)));
    let dispatch_index = handler.block.stmts.iter().position(|statement| {
        direct_statement_call(statement).as_deref() == Some("task::dispatch_pending_deferred_work")
    });
    if !matches!((match_index, dispatch_index), (Some(event), Some(dispatch)) if dispatch == event + 1)
    {
        errors.push(format!(
            "{TRAP_SOURCE}: every user-trap path must consume deferred work at the common user-return safe point immediately after event handling"
        ));
    }
}

fn check_idle_dispatch_is_irq_closed(source: &SourceFile, errors: &mut Vec<String>) {
    let Some(run_tasks) = function(source, "run_tasks") else {
        errors.push(format!("{PROCESS_TABLE_SOURCE}: missing run_tasks"));
        return;
    };
    let body = run_tasks.block.to_token_stream().to_string();
    let irq = body.find("LocalIrqGuard :: disable");
    let dispatch = body.find("scheduler_deferred_safe_point");
    let select = body.find("Processor :: select_task");
    let resume_tick = body.find("resume_local_idle_tick");
    let switch = body.find("switch_from_idle");
    let suspend_tick = body.find("suspend_local_idle_tick");
    let wait = body.find("wait_with_local_irq_masked");
    let restore = wait.and_then(|wait| {
        body[wait..]
            .find("drop (idle_irq)")
            .map(|offset| wait + offset)
    });
    if !matches!(
        (
            irq,
            dispatch,
            select,
            resume_tick,
            switch,
            suspend_tick,
            wait,
            restore,
        ),
        (
            Some(irq),
            Some(dispatch),
            Some(select),
            Some(resume_tick),
            Some(switch),
            Some(suspend_tick),
            Some(wait),
            Some(restore),
        ) if irq < dispatch
            && dispatch < select
            && select < resume_tick
            && resume_tick < switch
            && switch < suspend_tick
            && suspend_tick < wait
            && wait < restore
    ) {
        errors.push(format!(
            "{PROCESS_TABLE_SOURCE}: idle scheduler must resume the local tick before task switch and suspend it before exact-PC WFI while the local IRQ guard remains held"
        ));
    }
}

#[derive(Default)]
struct CallVisitor {
    names: Vec<String>,
}

impl<'ast> Visit<'ast> for CallVisitor {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let Expr::Path(path) = &*call.func {
            self.names.push(
                path.path
                    .segments
                    .iter()
                    .map(|segment| segment.ident.to_string())
                    .collect::<Vec<_>>()
                    .join("::"),
            );
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_impl_item_fn(&mut self, function: &'ast ImplItemFn) {
        syn::visit::visit_impl_item_fn(self, function);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository_sources() -> Vec<SourceFile> {
        let root = super::super::repository_root();
        super::super::load_sources(&root).expect("repository sources")
    }

    fn mutate(sources: &mut [SourceFile], path: &str, before: &str, after: &str) {
        let source = sources
            .iter_mut()
            .find(|source| source.relative == path)
            .expect("contract mutation source");
        assert!(source.text.contains(before), "mutation anchor missing");
        source.text = source.text.replacen(before, after, 1);
        source.lines = source.text.lines().map(str::to_owned).collect();
        source.syntax = syn::parse_file(&source.text).expect("mutated source must parse");
    }

    fn errors(sources: &[SourceFile]) -> Vec<String> {
        let mut errors = Vec::new();
        check(sources, &mut errors);
        errors
    }

    #[test]
    fn driver_io_dispatch_is_lock_free() {
        let mut sources = repository_sources();
        assert!(errors(&sources).is_empty(), "{:#?}", errors(&sources));
        mutate(
            &mut sources,
            DRIVER_REGISTRY_SOURCE,
            ".filter_map(Once::get)",
            ".filter_map(Once::get).map(|source| { let _ = NETWORK_DEVICES.devices.lock(); source })",
        );
        assert!(
            errors(&sources)
                .iter()
                .any(|error| error.contains("completion dispatch must read sources lock-free")),
            "{:#?}",
            errors(&sources)
        );
    }

    #[derive(Default)]
    struct SoftwareInterruptModel {
        deferred: bool,
        deferred_notifications: usize,
        request: u64,
        completion: u64,
        ssip: bool,
    }

    impl SoftwareInterruptModel {
        fn publish_deferred(&mut self) {
            if !self.deferred {
                self.deferred_notifications += 1;
                self.ssip = true;
            }
            self.deferred = true;
        }

        fn publish_remote_barrier(&mut self, generation: u64) {
            self.request = generation;
            self.ssip = true;
        }

        fn take_deferred(&mut self) {
            self.deferred = false;
        }

        fn handle_software_interrupt(&mut self) {
            assert!(self.ssip);
            self.ssip = false;
            self.completion = self.request;
        }
    }

    #[test]
    fn repository_obeys_deferred_context_contract() {
        let sources = repository_sources();
        let errors = errors(&sources);
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }

    #[test]
    fn kernel_software_interrupt_cannot_dispatch_domains() {
        let mut sources = repository_sources();
        mutate(
            &mut sources,
            TRAP_SOURCE,
            "    crate::task::complete_pending_memory_barrier();",
            "    crate::task::complete_pending_memory_barrier();\n    task::dispatch_pending_deferred_work();",
        );
        assert!(errors(&sources).iter().any(|error| {
            error.contains("supervisor software interrupt must first acknowledge SSIP")
        }));
    }

    #[test]
    fn deferred_take_cannot_ack_the_shared_ssip() {
        let mut sources = repository_sources();
        mutate(
            &mut sources,
            CPU_DEFERRED_SOURCE,
            "    DeferredWorkSet(pending.swap(0, Ordering::AcqRel))",
            "    crate::arch::interrupt::clear_software();\n    DeferredWorkSet(pending.swap(0, Ordering::AcqRel))",
        );
        assert!(
            errors(&sources)
                .iter()
                .any(|error| { error.contains("SSIP acknowledgement must remain unique") })
        );
    }

    #[test]
    fn remote_barrier_edge_survives_deferred_safe_point() {
        let mut model = SoftwareInterruptModel::default();
        model.publish_deferred();
        model.publish_remote_barrier(7);
        model.take_deferred();
        assert!(model.ssip, "safe point must preserve the shared IPI edge");
        model.handle_software_interrupt();
        assert_eq!(model.completion, 7);
    }

    #[test]
    fn repeated_deferred_publication_coalesces_one_local_edge() {
        let mut model = SoftwareInterruptModel::default();
        model.publish_deferred();
        model.publish_deferred();
        assert_eq!(model.deferred_notifications, 1);
        model.take_deferred();
        model.publish_deferred();
        assert_eq!(model.deferred_notifications, 2);
    }

    #[test]
    fn deferred_publisher_cannot_notify_on_an_already_pending_bitmap() {
        let mut sources = repository_sources();
        mutate(
            &mut sources,
            CPU_DEFERRED_SOURCE,
            "    if previous == 0 {",
            "    if previous != 0 {",
        );
        assert!(
            errors(&sources)
                .iter()
                .any(|error| { error.contains("empty-to-nonempty transition") })
        );
    }

    #[test]
    fn user_return_safe_point_cannot_be_removed() {
        let mut sources = repository_sources();
        mutate(
            &mut sources,
            TRAP_SOURCE,
            "    task::dispatch_pending_deferred_work();",
            "",
        );
        assert!(
            errors(&sources)
                .iter()
                .any(|error| error.contains("common user-return safe point"))
        );
    }

    #[test]
    fn virtio_hardirq_cannot_enter_queue_state() {
        let mut sources = repository_sources();
        mutate(
            &mut sources,
            "kernel/src/devices/virtio/net.rs",
            "        let status = self",
            "        self.queues.lock();\n        let status = self",
        );
        assert!(
            errors(&sources)
                .iter()
                .any(|error| error.contains("VirtIO hardirq must publish"))
        );
    }

    #[test]
    fn adapter_cannot_add_an_irq_lock_track() {
        let mut sources = repository_sources();
        mutate(
            &mut sources,
            "kernel/src/devices/virtio/input.rs",
            "events: Mutex<EventQueueState>",
            "events: IrqMutex<EventQueueState>",
        );
        assert!(
            errors(&sources)
                .iter()
                .any(|error| error.contains("sole ordinary safe-point lock"))
        );
    }

    #[test]
    fn kernel_space_cannot_add_an_irq_lock_track() {
        let mut sources = repository_sources();
        mutate(
            &mut sources,
            "kernel/src/memory/mod.rs",
            "Once<Mutex<MemorySet>>",
            "Once<IrqMutex<MemorySet>>",
        );
        assert!(
            errors(&sources)
                .iter()
                .any(|error| error.contains("KERNEL_SPACE must remain"))
        );
    }
}
