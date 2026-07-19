use std::{fs, path::Path};

const RNG_SOURCE: &str = "kernel/src/drivers/virtio_rng.rs";
const BLOCK_SOURCE: &str = "kernel/src/drivers/block.rs";
const VIRTIO_BLOCK_SOURCE: &str = "kernel/src/drivers/virtio_blk.rs";
const IO_COMPLETION_SOURCE: &str = "kernel/src/drivers/io_completion.rs";
const WAIT_COMPLETION_SOURCE: &str = "kernel/src/sync/wait_completion.rs";
const VIRTIO_IRQ_SOURCE: &str = "kernel/src/drivers/virtio_completion_irq.rs";
const GETRANDOM_SOURCE: &str = "kernel/src/syscall/random.rs";
const READ_SOURCE: &str = "kernel/src/syscall/fs/io/sequential/read.rs";

const DELAYED_POLLS: usize = 64;
const MODEL_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RngIoCost {
    pub(super) completion_owner_tracks: usize,
    pub(super) delayed_mmio_reads: usize,
    pub(super) spin_iterations: usize,
    pub(super) deferred_paths: usize,
    pub(super) getrandom_batches: usize,
    pub(super) dev_random_batches: usize,
    pub(super) preinitialized_output_bytes: usize,
    pub(super) preinitialized_dma_bytes: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(cost) if within_budget(cost) => {}
        Ok(cost) => errors.push(format!(
            "{RNG_SOURCE}: RNG must share one driver-I/O completion owner, sleep/WFI outside hardirq, reclaim through one deferred path, and copy initialized 4KiB batches; fixed {DELAYED_POLLS}-poll/{MODEL_BYTES}-byte model measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn within_budget(cost: RngIoCost) -> bool {
    cost == (RngIoCost {
        completion_owner_tracks: 1,
        delayed_mmio_reads: 0,
        spin_iterations: 0,
        deferred_paths: 1,
        getrandom_batches: 16,
        dev_random_batches: 16,
        preinitialized_output_bytes: 0,
        preinitialized_dma_bytes: 0,
    })
}

pub(super) fn measure(root: &Path) -> Result<RngIoCost, String> {
    let rng = read(root, RNG_SOURCE)?;
    if !bootstrap_completion_precedes_wfi(&rng) {
        return Err(format!(
            "{RNG_SOURCE}: bootstrap wait must reclaim an already-published completion before WFI"
        ));
    }
    let block = read(root, BLOCK_SOURCE)?;
    let virtio_block = read(root, VIRTIO_BLOCK_SOURCE)?;
    let io_completion = read(root, IO_COMPLETION_SOURCE).unwrap_or_default();
    let wait_completion = read(root, WAIT_COMPLETION_SOURCE).unwrap_or_default();
    let virtio_irq = read(root, VIRTIO_IRQ_SOURCE).unwrap_or_default();
    let getrandom = read(root, GETRANDOM_SOURCE)?;
    let read_source = read(root, READ_SOURCE)?;

    let completion_owner_tracks = usize::from(block.contains("Completion(AtomicU8"))
        + usize::from(rng.contains("Ok(None) => core::hint::spin_loop()"))
        + usize::from(
            io_completion.contains("type IoCompletion = WaitCompletion")
                && wait_completion.contains("struct WaitCompletion"),
        );
    let delayed_mmio_reads = rng
        .matches("self.device.interrupt_status().unwrap_or(0)")
        .count()
        * DELAYED_POLLS;
    let spin_iterations = rng.matches("spin_loop()").count() * DELAYED_POLLS;
    let deferred_paths = usize::from(
        virtio_block.contains("acknowledge_and_defer")
            && rng.contains("acknowledge_and_defer")
            && rng.contains("dispatch_completion_work")
            && virtio_irq.contains("Err(_) => true")
            && virtio_irq.contains("DeferredWork::DriverIo"),
    );
    let getrandom_batches = batches(&getrandom);
    let dev_random_batches = batches(&read_source);
    let preinitialized_output_bytes = usize::from(getrandom.contains("[0u8; 256]")) * MODEL_BYTES
        + usize::from(read_source.contains("[0u8; 256]")) * MODEL_BYTES;
    let preinitialized_dma_bytes = usize::from(rng.contains("DmaBuffer::try_zeroed()")) * 4096;

    Ok(RngIoCost {
        completion_owner_tracks,
        delayed_mmio_reads,
        spin_iterations,
        deferred_paths,
        getrandom_batches,
        dev_random_batches,
        preinitialized_output_bytes,
        preinitialized_dma_bytes,
    })
}

fn bootstrap_completion_precedes_wfi(source: &str) -> bool {
    let Some(wait) = source
        .split_once("fn wait(&self, identity: RequestIdentity)")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| suffix.split_once("fn finish(").map(|(wait, _)| wait))
    else {
        return false;
    };
    wait.find("has_used()")
        .zip(wait.find("acknowledge_and_defer"))
        .zip(wait.find("self.reclaim_completions()"))
        .zip(wait.find("wait_for_external_interrupt()"))
        .is_some_and(|(((used, ack), reclaim), sleep)| {
            used < ack && ack < reclaim && reclaim < sleep
        })
}

fn batches(source: &str) -> usize {
    if source.contains("EntropyBatch::<4096>") {
        MODEL_BYTES / 4096
    } else if source.contains("[0u8; 256]") {
        MODEL_BYTES / 256
    } else {
        MODEL_BYTES
    }
}

fn read(root: &Path, path: &str) -> Result<String, String> {
    fs::read_to_string(root.join(path)).map_err(|error| format!("{path}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_rng_has_one_completion_and_initialized_batch_track() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("RNG I/O cost must be measurable");
        assert!(within_budget(cost), "measured {cost:?}");
    }

    #[test]
    fn bootstrap_completion_is_checked_before_wfi() {
        let root = super::super::repository_root();
        let rng = read(&root, RNG_SOURCE).unwrap();
        assert!(bootstrap_completion_precedes_wfi(&rng));
    }
}
