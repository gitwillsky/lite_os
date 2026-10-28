//! Cross-OFD PRIME ownership and implicit synchronization on the production kernel, without UI.
use linux_uapi::drm::{
    CommandEncoder, DrmDevice, FORMAT_B8G8R8A8_UNORM, FORMAT_B8G8R8X8_UNORM, ObjectKind,
};
use std::{io, os::fd::AsFd};
fn step<T>(phase: &str, result: io::Result<T>) -> Result<T, String> {
    result.map_err(|error| format!("PRIME {phase}: {error}"))
}

/// Verify exported resources survive their producer's OFD/context, repeat/self import returns
/// the same file-local handle, and format aliases do not double-close their shared GEM owner.
pub(super) fn verify() -> Result<(), String> {
    let producer = step("producer open", DrmDevice::open("/dev/dri/renderD128"))?;
    if producer.set_master().is_ok() {
        return Err("render node acquired KMS master".into());
    }
    let graphics = step(
        "producer context",
        producer.initialize_virgl("prime-producer"),
    )?;
    let resource = step("allocate", graphics.create_texture(16, 16))?;
    let mut commands = CommandEncoder::new();
    commands.create_surface(8, resource.resource_id(), resource.format());
    commands.set_framebuffer(8);
    commands.set_viewport(16, 16);
    commands.clear([0.25, 0.5, 0.75, 1.]);
    commands.destroy_object(ObjectKind::Surface, 8);
    step(
        "producer exec",
        graphics.exec(commands.words(), &[&resource]),
    )?;
    let fd = step("export", producer.export_dma_buf(resource.handle()))?;
    let duplicate = step("duplicate fd", fd.try_clone())?;
    let self_handle = step("self import", producer.import_dma_buf(fd.as_fd()))?;
    if self_handle != resource.handle() {
        return Err("self import allocated a second GEM handle".into());
    }
    let (_, producer_memory, producer_shared) =
        step("producer fdinfo", super::fdinfo::read(producer.as_fd()))?;
    if producer_memory != 4 {
        return Err("self import changed page-aligned GPU memory accounting".into());
    }
    if producer_shared != producer_memory {
        return Err("exported GEM is not reported as drm-shared-memory".into());
    }
    let consumer = step("consumer open", DrmDevice::open("/dev/dri/renderD128"))?;
    let imported_graphics = step(
        "consumer context",
        consumer.initialize_virgl("prime-consumer"),
    )?;
    let imported = step(
        "import",
        imported_graphics.import_texture(fd.as_fd(), 16, 16, 64, FORMAT_B8G8R8A8_UNORM),
    )?;
    let alias = step(
        "format alias",
        imported_graphics.import_texture(duplicate.as_fd(), 16, 16, 64, FORMAT_B8G8R8X8_UNORM),
    )?;
    if imported.handle() != alias.handle() || imported.resource_id() != resource.resource_id() {
        return Err("PRIME did not preserve resource/handle identity".into());
    }
    let (_, consumer_memory, consumer_shared) =
        step("consumer fdinfo", super::fdinfo::read(consumer.as_fd()))?;
    if consumer_memory != producer_memory {
        return Err("repeated PRIME import double-counted shared GPU memory".into());
    }
    if consumer_shared != consumer_memory {
        return Err("imported GEM is not reported as drm-shared-memory".into());
    }
    // The exported fd/consumer must retain the GEM object, not the producer's whole DRM file.
    drop(resource);
    if step(
        "producer close fdinfo",
        super::fdinfo::read(producer.as_fd()),
    )?
    .1 != 0
    {
        return Err("closed producer handle remains in GPU memory accounting".into());
    }
    drop(graphics);
    drop(producer);
    drop(imported);
    drop(fd);
    drop(duplicate);
    // Linux keeps `obj->dma_buf` until the last handle closes, so the consumer stays shared.
    if step(
        "consumer fdinfo after export close",
        super::fdinfo::read(consumer.as_fd()),
    )?
    .2 != producer_memory
    {
        return Err("closing the dma-buf fd made a PRIME GEM private".into());
    }
    // No producer wait: as on Linux, host control-queue order runs the consumer transfer after the
    // producer's earlier draw; only the consumer's own result needs the following wait.
    step(
        "consumer transfer after exporter close",
        alias.transfer_from_host(0, 0, 16, 16),
    )?;
    step("consumer wait", alias.wait())?;
    let mut color = [0];
    alias.read_row(0, 0, &mut color);
    let [color] = color;
    for (shift, expected) in [(16, 64u8), (8, 128), (0, 191)] {
        if ((color >> shift) as u8).abs_diff(expected) > 2 {
            return Err(format!("PRIME readback after close: {color:#010x}"));
        }
    }
    drop(alias);
    drop(imported_graphics);
    drop(consumer);
    println!("drm-flip-probe: prime=ok exporter-close=ok implicit-sync=ok aliases=ok");
    Ok(())
}
