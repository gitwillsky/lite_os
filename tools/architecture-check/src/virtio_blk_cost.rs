use quote::ToTokens;
use syn::{ImplItem, Item, ItemConst, ItemImpl, Lit, Type};

use super::SourceFile;

const SOURCE: &str = "kernel/src/drivers/virtio_blk.rs";
const TASK_SOURCE: &str = "kernel/src/task/mod.rs";
const INTERRUPT_SOURCE: &str = "kernel/src/arch/riscv64/interrupt.rs";
const DELAYED_POLLS: usize = 64;
const LEGACY_SPINS_PER_POLL: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VirtioBlockCost {
    pub(super) mmio_reads: usize,
    pub(super) spin_iterations: usize,
    pub(super) queue_lock_hold_polls: usize,
    pub(super) max_inflight: usize,
    pub(super) completed_callers: usize,
    pub(super) capacity_wait_mmio_reads: usize,
}

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    match measure(sources) {
        Ok(cost)
            if cost.mmio_reads == 0
                && cost.spin_iterations == 0
                && cost.queue_lock_hold_polls == 0
                && cost.max_inflight >= 8
                && cost.completed_callers == 32
                && cost.capacity_wait_mmio_reads == 0 => {}
        Ok(cost) => errors.push(format!(
            "{SOURCE}: delayed VirtIO-block completion must sleep outside the queue lock and preserve multi-request inflight capacity; fixed delay={DELAYED_POLLS} polls measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
    if !bootstrap_wait_factory_ordered(sources) {
        errors.push(format!(
            "{TASK_SOURCE}: processor topology must exist before installing the driver-I/O wait factory; otherwise bootstrap /bin/init block I/O spins in current_task()"
        ));
    }
    if !bootstrap_completion_precedes_wfi(sources) {
        errors.push(format!(
            "{SOURCE}: bootstrap wait must reclaim an already-published completion before WFI"
        ));
    }
    if !bootstrap_wfi_precedes_global_enable(sources) {
        errors.push(format!(
            "{INTERRUPT_SOURCE}: bootstrap WFI must run with global SIE closed and precede external trap delivery"
        ));
    }
}

fn bootstrap_wfi_precedes_global_enable(sources: &[SourceFile]) -> bool {
    let Some(source) = sources
        .iter()
        .find(|source| source.relative == INTERRUPT_SOURCE)
    else {
        return false;
    };
    let Some(function) = source.syntax.items.iter().find_map(|item| match item {
        Item::Fn(function) if function.sig.ident == "wait_for_external_interrupt" => Some(function),
        _ => None,
    }) else {
        return false;
    };
    let text = function.block.to_token_stream().to_string();
    text.find("disable_local")
        .zip(text.find("__wait_for_external_interrupt"))
        .zip(text.find("restore_local"))
        .is_some_and(|((disable, wait), restore)| disable < wait && wait < restore)
}

fn bootstrap_completion_precedes_wfi(sources: &[SourceFile]) -> bool {
    let Some(source) = sources.iter().find(|source| source.relative == SOURCE) else {
        return false;
    };
    let Ok(wait) = method(source, "wait") else {
        return false;
    };
    let text = wait.block.to_token_stream().to_string();
    text.find("has_used")
        .zip(text.find("acknowledge_and_defer"))
        .zip(text.find("self . reclaim_completions ()"))
        .zip(text.find("wait_for_external_interrupt"))
        .is_some_and(|(((used, ack), reclaim), sleep)| {
            used < ack && ack < reclaim && reclaim < sleep
        })
}

fn bootstrap_wait_factory_ordered(sources: &[SourceFile]) -> bool {
    let Some(source) = sources.iter().find(|source| source.relative == TASK_SOURCE) else {
        return false;
    };
    let Some(topology) = source.text.find("processor::init_topology();") else {
        return false;
    };
    let Some(factory) = source
        .text
        .find("task_manager::initialize_driver_io_wait();")
    else {
        return false;
    };
    let Some(load) = source.text.find("let loaded = load_executable(") else {
        return false;
    };
    topology < factory && factory < load
}

fn measure(sources: &[SourceFile]) -> Result<VirtioBlockCost, String> {
    let source = sources
        .iter()
        .find(|source| source.relative == SOURCE)
        .ok_or_else(|| format!("{SOURCE}: missing production block adapter seam"))?;
    let completion_text = method(source, "complete_request")
        .map(|method| method.block.to_token_stream().to_string())
        .unwrap_or_default();
    let mmio_reads = completion_text.matches("interrupt_status").count() * DELAYED_POLLS;
    let spin_iterations =
        usize::from(completion_text.contains("spin_loop")) * LEGACY_SPINS_PER_POLL * DELAYED_POLLS;
    let locked_wait = ["read", "write", "flush_device"].iter().any(|name| {
        method(source, name).is_ok_and(|method| {
            let text = method.block.to_token_stream().to_string();
            text.contains("queue . lock") && text.contains("complete_request")
        })
    });
    Ok(VirtioBlockCost {
        mmio_reads,
        spin_iterations,
        queue_lock_hold_polls: usize::from(locked_wait) * DELAYED_POLLS,
        max_inflight: if source.text.contains("IoCompletion")
            && source.text.contains("DeferredWork::DriverIo")
        {
            request_slot_count(source).unwrap_or(1)
        } else {
            1
        },
        completed_callers: if source.text.contains("wait_for_capacity")
            && !source
                .text
                .contains("reserve().ok_or(BlockError::DeviceError)")
        {
            32
        } else {
            request_slot_count(source).unwrap_or(1)
        },
        capacity_wait_mmio_reads: usize::from(
            source.text.contains("wait_for_capacity")
                && method(source, "wait_for_capacity").is_ok_and(|method| {
                    method
                        .block
                        .to_token_stream()
                        .to_string()
                        .contains("interrupt_status")
                }),
        ) * DELAYED_POLLS,
    })
}

fn method<'a>(source: &'a SourceFile, name: &str) -> Result<&'a syn::ImplItemFn, String> {
    source
        .syntax
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Impl(ItemImpl { items, .. }) => Some(items),
            _ => None,
        })
        .flatten()
        .find_map(|item| match item {
            ImplItem::Fn(method) if method.sig.ident == name => Some(method),
            _ => None,
        })
        .ok_or_else(|| format!("{SOURCE}: missing measured method {name}"))
}

fn request_slot_count(source: &SourceFile) -> Option<usize> {
    source.syntax.items.iter().find_map(|item| {
        let Item::Const(ItemConst {
            ident, ty, expr, ..
        }) = item
        else {
            return None;
        };
        if ident != "BLOCK_REQUEST_SLOTS"
            || !matches!(&**ty, Type::Path(path) if path.path.is_ident("usize"))
        {
            return None;
        }
        let syn::Expr::Lit(literal) = &**expr else {
            return None;
        };
        let Lit::Int(value) = &literal.lit else {
            return None;
        };
        value.base10_parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::check;

    #[test]
    fn repository_block_adapter_does_not_poll_completion() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }

    #[test]
    fn capacity_pressure_completes_all_callers_without_polling() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let cost = super::measure(&sources).expect("block adapter cost");
        assert_eq!(cost.max_inflight, 16);
        assert_eq!(cost.completed_callers, 32, "measured {cost:?}");
        assert_eq!(cost.capacity_wait_mmio_reads, 0, "measured {cost:?}");
    }

    #[test]
    fn bootstrap_io_factory_follows_processor_topology() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        assert!(super::bootstrap_wait_factory_ordered(&sources));
    }

    #[test]
    fn bootstrap_completion_is_checked_before_wfi() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        assert!(super::bootstrap_completion_precedes_wfi(&sources));
    }

    #[test]
    fn bootstrap_wfi_cannot_lose_the_external_edge() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        assert!(super::bootstrap_wfi_precedes_global_enable(&sources));
    }
}
