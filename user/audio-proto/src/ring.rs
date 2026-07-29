use std::{
    io,
    marker::PhantomData,
    os::fd::{AsFd, OwnedFd},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{CHANNELS, LOW_WATER_FRAMES, RING_CAPACITY_FRAMES, RING_PCM_BYTES};
use linux_uapi::shared_memory::SharedMapping;

const RING_MAGIC: u64 = u64::from_le_bytes(*b"LTAUD001");
const HEADER_BYTES: usize = 64;

#[repr(C, align(64))]
struct RingHeader {
    magic: u64,
    capacity_frames: u64,
    generation: AtomicU64,
    produced_frames: AtomicU64,
    consumed_frames: AtomicU64,
    reserved: [u64; 3],
}

const _: () = assert!(size_of::<RingHeader>() == HEADER_BYTES);

/// Bytes required for one complete shared ring mapping.
pub const RING_MAPPING_BYTES: usize = HEADER_BYTES + RING_PCM_BYTES;

/// A shared-ring validation or ownership failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RingError {
    /// The mapping is too small or not aligned for its fixed header.
    InvalidMapping,
    /// The immutable ring identity was not initialized by the service.
    InvalidIdentity,
    /// The peer published impossible producer/consumer indices.
    CorruptIndices,
    /// The operation targets an inactive generation.
    StaleGeneration,
    /// PCM contained NaN or infinity and cannot enter the system mixer.
    InvalidSample,
}

/// An atomic snapshot of shared ownership indices.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingSnapshot {
    /// The active source generation.
    pub generation: u64,
    /// Total frames published by the producer in this generation.
    pub produced_frames: u64,
    /// Total frames released by the consumer in this generation.
    pub consumed_frames: u64,
}

/// Initializes a newly created, exclusively owned ring mapping.
///
/// # Parameters
///
/// - `base`: Aligned start of a writable mapping at least [`RING_MAPPING_BYTES`] bytes long.
/// - `length`: Total writable mapping length.
/// - `generation`: Initial nonzero media generation.
///
/// # Returns
///
/// `Ok(())` after publishing an empty ring, or [`RingError::InvalidMapping`].
///
/// # Safety
///
/// The caller must exclusively own the mapping during this call. `base` must
/// remain valid for `length` bytes and no Rust reference may alias the storage.
pub unsafe fn initialize_ring(
    base: NonNull<u8>,
    length: usize,
    generation: u64,
) -> Result<(), RingError> {
    if length < RING_MAPPING_BYTES
        || !(base.as_ptr() as usize).is_multiple_of(align_of::<RingHeader>())
        || generation == 0
    {
        return Err(RingError::InvalidMapping);
    }
    let header = base.cast::<RingHeader>().as_ptr();
    // SAFETY: The caller grants exclusive, aligned storage for a complete header.
    unsafe {
        header.write(RingHeader {
            magic: RING_MAGIC,
            capacity_frames: RING_CAPACITY_FRAMES as u64,
            generation: AtomicU64::new(generation),
            produced_frames: AtomicU64::new(0),
            consumed_frames: AtomicU64::new(0),
            reserved: [0; 3],
        });
    }
    Ok(())
}

struct Ring {
    header: NonNull<RingHeader>,
    samples: NonNull<f32>,
    _mapping: PhantomData<*mut u8>,
}

// SAFETY: Ring methods enforce the one-producer/one-consumer field ownership;
// moving a view to its dedicated worker thread does not create another owner.
unsafe impl Send for Ring {}

impl Ring {
    unsafe fn from_mapping(base: NonNull<u8>, length: usize) -> Result<Self, RingError> {
        if length < RING_MAPPING_BYTES
            || !(base.as_ptr() as usize).is_multiple_of(align_of::<RingHeader>())
        {
            return Err(RingError::InvalidMapping);
        }
        let header = base.cast::<RingHeader>();
        // SAFETY: The mapping bounds and header alignment were checked. Immutable
        // identity fields are written before SCM_RIGHTS publication and never mutate.
        let identity = unsafe {
            (
                std::ptr::read_volatile(std::ptr::addr_of!((*header.as_ptr()).magic)),
                std::ptr::read_volatile(std::ptr::addr_of!((*header.as_ptr()).capacity_frames)),
            )
        };
        if identity != (RING_MAGIC, RING_CAPACITY_FRAMES as u64) {
            return Err(RingError::InvalidIdentity);
        }
        // SAFETY: HEADER_BYTES is within the validated mapping and aligned for f32.
        let samples = unsafe { NonNull::new_unchecked(base.as_ptr().add(HEADER_BYTES).cast()) };
        Ok(Self {
            header,
            samples,
            _mapping: PhantomData,
        })
    }

    fn header(&self) -> &RingHeader {
        // SAFETY: The mapping outlives the view and the header remains initialized.
        unsafe { self.header.as_ref() }
    }

    fn snapshot(&self) -> Result<RingSnapshot, RingError> {
        let header = self.header();
        let snapshot = RingSnapshot {
            generation: header.generation.load(Ordering::Acquire),
            produced_frames: header.produced_frames.load(Ordering::Acquire),
            consumed_frames: header.consumed_frames.load(Ordering::Acquire),
        };
        let used = snapshot
            .produced_frames
            .checked_sub(snapshot.consumed_frames)
            .ok_or(RingError::CorruptIndices)?;
        if used > RING_CAPACITY_FRAMES as u64 {
            return Err(RingError::CorruptIndices);
        }
        Ok(snapshot)
    }

    unsafe fn sample_ptr(&self, frame: u64, channel: usize) -> *mut f32 {
        let slot = frame as usize % RING_CAPACITY_FRAMES;
        // SAFETY: slot and channel are bounded by ring constants.
        unsafe { self.samples.as_ptr().add(slot * CHANNELS + channel) }
    }
}

/// The audio-worker view of one shared SPSC ring.
pub struct ProducerRing(Ring);

impl ProducerRing {
    /// Projects a producer view over an initialized shared mapping.
    ///
    /// # Parameters
    ///
    /// - `base`: Stable start address of the shared mapping.
    /// - `length`: Total mapping length.
    ///
    /// # Returns
    ///
    /// A producer view or a mapping/identity error.
    ///
    /// # Safety
    ///
    /// The mapping must remain live and at the same address for the returned
    /// view's lifetime. No second producer may access its producer index.
    pub unsafe fn from_mapping(base: NonNull<u8>, length: usize) -> Result<Self, RingError> {
        // SAFETY: The caller establishes the mapping lifetime and unique producer.
        unsafe { Ring::from_mapping(base, length) }.map(Self)
    }

    /// Returns an acquire-ordered ownership snapshot.
    pub fn snapshot(&self) -> Result<RingSnapshot, RingError> {
        self.0.snapshot()
    }

    /// Copies as many interleaved stereo frames as fit and publishes them once.
    ///
    /// # Parameters
    ///
    /// - `generation`: Generation attached to the decoded PCM.
    /// - `frames`: Stereo float frames in the fixed 48 kHz normal form.
    ///
    /// # Returns
    ///
    /// The number of frames published and whether this was an empty-to-nonempty
    /// edge requiring [`crate::ClientMessage::RingNonempty`].
    pub fn write(
        &mut self,
        generation: u64,
        frames: &[[f32; CHANNELS]],
    ) -> Result<(usize, bool), RingError> {
        let snapshot = self.0.snapshot()?;
        if snapshot.generation != generation {
            return Err(RingError::StaleGeneration);
        }
        let used = (snapshot.produced_frames - snapshot.consumed_frames) as usize;
        let count = frames.len().min(RING_CAPACITY_FRAMES - used);
        for (offset, frame) in frames[..count].iter().enumerate() {
            for (channel, sample) in frame.iter().enumerate() {
                // SAFETY: This producer exclusively owns unpublished slots and its index.
                unsafe {
                    std::ptr::write_volatile(
                        self.0
                            .sample_ptr(snapshot.produced_frames + offset as u64, channel),
                        *sample,
                    );
                }
            }
        }
        self.0
            .header()
            .produced_frames
            .store(snapshot.produced_frames + count as u64, Ordering::Release);
        Ok((count, used == 0 && count != 0))
    }
}

/// Safe RAII producer mapping returned by a successful stream creation.
///
/// The wrapper consumes the sole descriptor published for the stream and keeps
/// both descriptor and mapping alive until all producer access has stopped.
pub struct MappedProducer {
    fd: OwnedFd,
    mapping: SharedMapping,
    ring: ProducerRing,
}

impl MappedProducer {
    /// Maps and validates one service-created ring descriptor.
    ///
    /// # Parameters
    ///
    /// - `fd`: The descriptor returned with [`crate::ServiceMessage::StreamCreated`].
    ///
    /// # Returns
    ///
    /// A unique producer mapping.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if mmap fails or invalid-data when the immutable
    /// ring identity is not the v1 8192-frame layout.
    pub fn map(fd: OwnedFd) -> io::Result<Self> {
        let mapping = SharedMapping::map(fd.as_fd(), RING_MAPPING_BYTES)?;
        // SAFETY: This wrapper consumes the protocol's sole producer descriptor,
        // owns the stable mapping, and does not expose a second producer view.
        let ring = unsafe { ProducerRing::from_mapping(mapping.as_non_null(), mapping.len()) }
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid audio ring"))?;
        Ok(Self { fd, mapping, ring })
    }

    /// Returns an acquire-ordered ownership snapshot.
    pub fn snapshot(&self) -> Result<RingSnapshot, RingError> {
        self.ring.snapshot()
    }

    /// Copies and publishes as many normal-form frames as fit.
    ///
    /// # Parameters
    ///
    /// - `generation`: Active media generation.
    /// - `frames`: 48 kHz interleaved stereo float frames.
    ///
    /// # Returns
    ///
    /// The published count and empty-to-nonempty edge flag.
    pub fn write(
        &mut self,
        generation: u64,
        frames: &[[f32; CHANNELS]],
    ) -> Result<(usize, bool), RingError> {
        self.ring.write(generation, frames)
    }

    /// Returns the fixed mapping size for diagnostics.
    pub fn mapping_len(&self) -> usize {
        let _ = self.fd.as_fd();
        self.mapping.len()
    }
}

/// The real-time mixer view of one shared SPSC ring.
pub struct ConsumerRing(Ring);

impl ConsumerRing {
    /// Projects a consumer view over an initialized shared mapping.
    ///
    /// # Parameters
    ///
    /// - `base`: Stable start address of the shared mapping.
    /// - `length`: Total mapping length.
    ///
    /// # Returns
    ///
    /// A consumer view or a mapping/identity error.
    ///
    /// # Safety
    ///
    /// The mapping must remain live and stable for the view's lifetime. No
    /// second consumer may access its consumer index.
    pub unsafe fn from_mapping(base: NonNull<u8>, length: usize) -> Result<Self, RingError> {
        // SAFETY: The caller establishes the mapping lifetime and unique consumer.
        unsafe { Ring::from_mapping(base, length) }.map(Self)
    }

    /// Returns an acquire-ordered ownership snapshot.
    pub fn snapshot(&self) -> Result<RingSnapshot, RingError> {
        self.0.snapshot()
    }

    /// Adds available frames to `output` with one cached stream gain.
    ///
    /// # Parameters
    ///
    /// - `generation`: Generation currently owned by the mixer registry.
    /// - `gain`: Standard linear amplitude gain.
    /// - `output`: Caller-owned, pre-cleared period mix buffer.
    ///
    /// # Returns
    ///
    /// The number of consumed frames and whether this consume crossed the ring
    /// down through [`crate::LOW_WATER_FRAMES`], requiring a
    /// [`crate::ServiceMessage::RingAvailable`] refill request. A level crossing
    /// (not an exactly-full observation) keeps refill re-arming under concurrent
    /// consumption, so a single late producer refill cannot permanently starve
    /// the stream.
    pub fn mix_into(
        &mut self,
        generation: u64,
        gain: f32,
        output: &mut [[f32; CHANNELS]],
    ) -> Result<(usize, bool), RingError> {
        let snapshot = self.0.snapshot()?;
        if snapshot.generation != generation {
            return Err(RingError::StaleGeneration);
        }
        let available = (snapshot.produced_frames - snapshot.consumed_frames) as usize;
        let count = available.min(output.len());
        for (offset, output_frame) in output[..count].iter_mut().enumerate() {
            for (channel, output_sample) in output_frame.iter_mut().enumerate() {
                // SAFETY: Acquire of produced_frames makes published PCM visible;
                // this consumer alone reads/relinquishes the consumed slots.
                let sample = unsafe {
                    std::ptr::read_volatile(
                        self.0
                            .sample_ptr(snapshot.consumed_frames + offset as u64, channel),
                    )
                };
                if !sample.is_finite() {
                    return Err(RingError::InvalidSample);
                }
                *output_sample += sample * gain;
            }
        }
        self.0
            .header()
            .consumed_frames
            .store(snapshot.consumed_frames + count as u64, Ordering::Release);
        // Downward crossing of the refill watermark: fires once as `available`
        // moves from above the low-water line to at-or-below it. Edge, not level,
        // so a steady drain emits exactly one request per refill cycle.
        let crossed =
            count != 0 && available > LOW_WATER_FRAMES && available - count <= LOW_WATER_FRAMES;
        Ok((count, crossed))
    }

    /// Discards all published PCM and installs one newer generation.
    ///
    /// # Parameters
    ///
    /// - `old_generation`: Generation whose consumption has stopped.
    /// - `new_generation`: Strictly newer generation to publish empty.
    ///
    /// # Returns
    ///
    /// `Ok(())` after both indices and generation are atomically ordered for the
    /// producer, or a stale/corrupt error.
    pub fn flush(&mut self, old_generation: u64, new_generation: u64) -> Result<(), RingError> {
        let snapshot = self.0.snapshot()?;
        if snapshot.generation != old_generation || new_generation <= old_generation {
            return Err(RingError::StaleGeneration);
        }
        let header = self.0.header();
        header
            .consumed_frames
            .store(snapshot.produced_frames, Ordering::Release);
        header.produced_frames.store(0, Ordering::Release);
        header.consumed_frames.store(0, Ordering::Release);
        header.generation.store(new_generation, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(align(64))]
    struct Storage([u8; RING_MAPPING_BYTES]);

    fn rings(generation: u64) -> (Box<Storage>, ProducerRing, ConsumerRing) {
        let mut storage = Box::new(Storage([0; RING_MAPPING_BYTES]));
        let base = NonNull::new(storage.0.as_mut_ptr()).expect("storage pointer");
        // SAFETY: Box owns stable, aligned storage for both views during the test.
        unsafe { initialize_ring(base, RING_MAPPING_BYTES, generation) }.expect("initialize");
        let producer =
            unsafe { ProducerRing::from_mapping(base, RING_MAPPING_BYTES) }.expect("producer");
        let consumer =
            unsafe { ConsumerRing::from_mapping(base, RING_MAPPING_BYTES) }.expect("consumer");
        (storage, producer, consumer)
    }

    #[test]
    fn spsc_wrap_preserves_order_and_edges() {
        let (_storage, mut producer, mut consumer) = rings(7);
        let first = vec![[0.25, -0.25]; RING_CAPACITY_FRAMES];
        assert_eq!(producer.write(7, &first), Ok((RING_CAPACITY_FRAMES, true)));
        assert_eq!(producer.write(7, &[[1.0, 1.0]]), Ok((0, false)));

        let mut half = vec![[0.0; CHANNELS]; RING_CAPACITY_FRAMES / 2];
        assert_eq!(
            consumer.mix_into(7, 2.0, &mut half),
            Ok((RING_CAPACITY_FRAMES / 2, true))
        );
        assert!(half.iter().all(|frame| *frame == [0.5, -0.5]));

        let wrapped = vec![[0.75, 0.5]; RING_CAPACITY_FRAMES / 2];
        assert_eq!(
            producer.write(7, &wrapped),
            Ok((RING_CAPACITY_FRAMES / 2, false))
        );
        let mut output = vec![[0.0; CHANNELS]; RING_CAPACITY_FRAMES];
        assert_eq!(
            consumer.mix_into(7, 1.0, &mut output),
            Ok((RING_CAPACITY_FRAMES, true))
        );
        assert!(
            output[..RING_CAPACITY_FRAMES / 2]
                .iter()
                .all(|frame| *frame == [0.25, -0.25])
        );
        assert!(
            output[RING_CAPACITY_FRAMES / 2..]
                .iter()
                .all(|frame| *frame == [0.75, 0.5])
        );
    }

    #[test]
    fn flush_rejects_late_generation_and_empties_ring() {
        let (_storage, mut producer, mut consumer) = rings(3);
        producer.write(3, &[[1.0, 1.0]; 16]).expect("write");
        consumer.flush(3, 4).expect("flush");
        assert_eq!(
            producer.write(3, &[[1.0, 1.0]]),
            Err(RingError::StaleGeneration)
        );
        assert_eq!(producer.snapshot().expect("snapshot").generation, 4);
        assert_eq!(producer.write(4, &[[0.5, 0.5]]), Ok((1, true)));
    }

    #[test]
    fn consumer_rejects_non_finite_pcm() {
        let (_storage, mut producer, mut consumer) = rings(1);
        producer.write(1, &[[f32::NAN, 0.0]]).expect("publish");
        assert_eq!(
            consumer.mix_into(1, 1.0, &mut [[0.0; CHANNELS]; 1]),
            Err(RingError::InvalidSample)
        );
    }
}
