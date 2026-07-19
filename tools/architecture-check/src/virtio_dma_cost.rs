use quote::ToTokens;
use syn::{ImplItem, Item, ItemImpl, Type};

use super::SourceFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DmaSubmissionCost {
    pub(super) net_rx_locks: usize,
    pub(super) net_tx_locks: usize,
    pub(super) input_locks: usize,
    pub(super) gpu_locks: usize,
    pub(super) block_locks: usize,
    pub(super) page_walks: usize,
}

impl DmaSubmissionCost {
    fn is_zero(self) -> bool {
        self.net_rx_locks == 0
            && self.net_tx_locks == 0
            && self.input_locks == 0
            && self.gpu_locks == 0
            && self.block_locks == 0
            && self.page_walks == 0
    }
}

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    match measure(sources) {
        Ok(cost)
            if cost.is_zero() && cached_dma_only(sources) && reset_before_dma_drop(sources) => {}
        Ok(cost) => errors.push(format!(
            "steady-state VirtIO submission must consume initialization-cached DMA segments only; measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(sources: &[SourceFile]) -> Result<DmaSubmissionCost, String> {
    let net = source(sources, "kernel/src/drivers/virtio_net.rs")?;
    let input = source(sources, "kernel/src/drivers/virtio_input.rs")?;
    let gpu = source(sources, "kernel/src/drivers/virtio_gpu.rs")?;
    let damage = source(sources, "kernel/src/drivers/virtio_gpu/damage.rs")?;
    let block = source(sources, "kernel/src/drivers/virtio_blk.rs")?;
    let net_rx_locks = runtime_translation(method_text(net, "repost")?);
    let net_tx_locks = runtime_translation(method_text(net, "submit_transmit")?);
    let input_locks = runtime_translation(method_text(input, "receive_event")?);
    let gpu_locks = runtime_translation(method_text(gpu, "publish_prepared")?)
        + runtime_translation(method_text(damage, "publish_next")?);
    let block_locks = runtime_translation(method_text(block, "submit")?);
    Ok(DmaSubmissionCost {
        net_rx_locks,
        net_tx_locks,
        input_locks,
        gpu_locks,
        block_locks,
        page_walks: net_rx_locks * 2
            + net_tx_locks * 2
            + input_locks * 2
            + gpu_locks * 4
            + block_locks * 5,
    })
}

fn source<'a>(sources: &'a [SourceFile], path: &str) -> Result<&'a SourceFile, String> {
    sources
        .iter()
        .find(|source| source.relative == path)
        .ok_or_else(|| format!("{path}: missing VirtIO adapter source"))
}

fn method_text(source: &SourceFile, name: &str) -> Result<String, String> {
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
            ImplItem::Fn(method) if method.sig.ident == name => {
                Some(method.block.to_token_stream().to_string())
            }
            _ => None,
        })
        .ok_or_else(|| format!("{}: missing measured method {name}", source.relative))
}

fn runtime_translation(method: String) -> usize {
    usize::from(method.contains("add_buffer"))
}

fn cached_dma_only(sources: &[SourceFile]) -> bool {
    source(sources, "kernel/src/drivers/virtio_queue.rs").is_ok_and(|queue| {
        queue.text.contains("DmaBuffer")
            && queue.text.contains("add_dma")
            && !queue.text.contains("fn add_buffer")
            && !queue.text.contains("KERNEL_SPACE")
    })
}

fn reset_before_dma_drop(sources: &[SourceFile]) -> bool {
    let transport_waits = source(sources, "kernel/src/drivers/hal/virtio.rs").is_ok_and(|source| {
        method_text(source, "reset")
            .is_ok_and(|method| method.contains("while self . get_status () ? != 0"))
    });
    transport_waits
        && [
            ("kernel/src/drivers/virtio_net.rs", "VirtIONetworkDevice"),
            ("kernel/src/drivers/virtio_input.rs", "VirtIOInputDevice"),
            ("kernel/src/drivers/virtio_gpu.rs", "VirtIOGpuDevice"),
            ("kernel/src/drivers/virtio_blk.rs", "VirtIOBlockDevice"),
            ("kernel/src/drivers/virtio_rng.rs", "VirtIORngDevice"),
        ]
        .into_iter()
        .all(|(path, owner)| {
            source(sources, path).is_ok_and(|source| has_reset_drop(source, owner))
        })
}

fn has_reset_drop(source: &SourceFile, owner: &str) -> bool {
    source.syntax.items.iter().any(|item| {
        let Item::Impl(item) = item else {
            return false;
        };
        let is_drop = item
            .trait_
            .as_ref()
            .and_then(|(_, path, _)| path.segments.last())
            .is_some_and(|segment| segment.ident == "Drop");
        let is_owner = match item.self_ty.as_ref() {
            Type::Path(path) => path
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == owner),
            _ => false,
        };
        is_drop
            && is_owner
            && item.items.iter().any(|method| match method {
                ImplItem::Fn(method) if method.sig.ident == "drop" => method
                    .block
                    .to_token_stream()
                    .to_string()
                    .contains("reset ()"),
                _ => false,
            })
    })
}

#[cfg(test)]
mod tests {
    use super::{cached_dma_only, measure, reset_before_dma_drop};

    #[test]
    fn steady_state_submission_has_no_translation_lock_or_page_walk() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let cost = measure(&sources).expect("VirtIO DMA cost");
        assert!(cost.is_zero(), "measured {cost:?}");
        assert!(
            cached_dma_only(&sources),
            "runtime translation fallback remains"
        );
        assert!(
            reset_before_dma_drop(&sources),
            "adapter can drop cached DMA storage before device reset"
        );
    }
}
