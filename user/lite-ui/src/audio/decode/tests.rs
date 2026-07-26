use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use super::{
    BoundedFile, DecoderSession, OUTPUT_CHANNELS, OUTPUT_RATE, SincResampler, normalize_stereo,
};

const FIXTURES: [&str; 13] = [
    "tone.wav",
    "tone.aiff",
    "tone.caf",
    "tone.flac",
    "tone.mp1",
    "tone.mp2",
    "tone.mp3",
    "tone.ogg",
    "tone-aac.m4a",
    "tone.mp4",
    "tone-alac.m4a",
    "tone.mka",
    "tone.webm",
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/fixtures/audio")
        .join(name)
}

#[test]
fn every_supported_container_and_codec_decodes_and_seeks_through_the_production_path() {
    for name in FIXTURES {
        let path = fixture(name);
        let length = path.metadata().expect("fixture metadata").len();
        let mut decoder = DecoderSession::open_range(&path, 0, length)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let duration = decoder.metadata().duration;
        assert!(
            duration.is_finite() && duration > 0.0,
            "{name}: invalid duration {duration}"
        );

        let mut quantum = [0.0; 128 * OUTPUT_CHANNELS];
        let mut decoded_frames = 0usize;
        let mut has_signal = false;
        loop {
            let frames = decoder
                .read_quantum(&mut quantum)
                .unwrap_or_else(|error| panic!("{name}: sequential decode failed: {error}"));
            if frames == 0 {
                break;
            }
            decoded_frames += frames;
            let samples = &quantum[..frames * OUTPUT_CHANNELS];
            assert!(
                samples.iter().all(|sample| sample.is_finite()),
                "{name}: decoder produced a non-finite sample"
            );
            has_signal |= samples.iter().any(|sample| sample.abs() > 1.0e-5);
            assert!(
                decoded_frames <= OUTPUT_RATE as usize * 10,
                "{name}: decoder did not reach EOF within the fixture bound"
            );
        }
        assert!(decoded_frames > 0, "{name}: decoded no PCM frames");
        assert!(has_signal, "{name}: decoded output was silent");

        decoder
            .seek(duration * 0.5)
            .unwrap_or_else(|error| panic!("{name}: seek failed: {error}"));
        let frames = decoder
            .read_quantum(&mut quantum)
            .unwrap_or_else(|error| panic!("{name}: post-seek decode failed: {error}"));
        assert!(frames > 0, "{name}: seek landed at EOF");
        let samples = &quantum[..frames * OUTPUT_CHANNELS];
        assert!(
            samples.iter().all(|sample| sample.is_finite()),
            "{name}: post-seek decoder produced a non-finite sample"
        );
        assert!(
            samples.iter().any(|sample| sample.abs() > 1.0e-5),
            "{name}: post-seek output was silent"
        );
    }
}

#[test]
fn every_supported_container_rejects_truncated_and_corrupt_input() {
    for name in FIXTURES {
        let bytes = fs::read(fixture(name)).unwrap_or_else(|error| panic!("{name}: {error}"));
        let extension = Path::new(name)
            .extension()
            .and_then(|value| value.to_str())
            .expect("fixture extension");
        let base = format!(
            "lite-ui-codec-{}-{}",
            std::process::id(),
            name.replace('.', "-")
        );
        let truncated = std::env::temp_dir().join(format!("{base}-truncated.{extension}"));
        let corrupt = std::env::temp_dir().join(format!("{base}-corrupt.{extension}"));
        fs::write(&truncated, &bytes[..bytes.len().min(16)]).expect("truncated fixture");
        fs::write(&corrupt, vec![0xa5; bytes.len().min(4096)]).expect("corrupt fixture");

        assert_rejected(name, "truncated", &truncated);
        assert_rejected(name, "corrupt", &corrupt);
        fs::remove_file(truncated).expect("remove truncated fixture");
        fs::remove_file(corrupt).expect("remove corrupt fixture");
    }
}

#[test]
fn every_codec_stabilizes_all_working_capacities_by_prefill_midpoint() {
    for name in FIXTURES {
        let path = fixture(name);
        let length = path.metadata().expect("fixture metadata").len();
        let mut decoder = DecoderSession::open_range(&path, 0, length)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let mut quantum = [0.0; 128 * OUTPUT_CHANNELS];
        for _ in 0..32 {
            assert_eq!(
                decoder
                    .read_quantum(&mut quantum)
                    .unwrap_or_else(|error| panic!("{name}: warmup failed: {error}")),
                128,
                "{name}: ended during allocation warmup"
            );
        }
        let warm_epoch = decoder.allocation_epoch();
        let warm_capacities = decoder.working_capacities();
        for _ in 0..32 {
            assert_eq!(
                decoder
                    .read_quantum(&mut quantum)
                    .unwrap_or_else(|error| panic!("{name}: steady decode failed: {error}")),
                128,
                "{name}: ended during steady allocation window"
            );
        }
        assert_eq!(
            decoder.allocation_epoch(),
            warm_epoch,
            "{name}: capacity changed after warmup: {:?} -> {:?}",
            warm_capacities,
            decoder.working_capacities()
        );
        assert_eq!(
            decoder.working_capacities(),
            warm_capacities,
            "{name}: working capacity changed without an allocation epoch"
        );
    }
}

fn assert_rejected(name: &str, kind: &str, path: &Path) {
    let length = path.metadata().expect("malformed metadata").len();
    match DecoderSession::open_range(path, 0, length) {
        Err(_) => {}
        Ok(mut decoder) => {
            let mut quantum = [0.0; 128 * OUTPUT_CHANNELS];
            loop {
                match decoder.read_quantum(&mut quantum) {
                    Err(_) => break,
                    Ok(0) => panic!("{name}: {kind} input was accepted as complete media"),
                    Ok(_) => {}
                }
            }
        }
    }
}

#[test]
fn every_codec_seeks_after_live_refill() {
    for name in FIXTURES {
        for target_fraction in [0.0, 0.5] {
            let path = fixture(name);
            let length = path.metadata().expect("fixture metadata").len();
            let mut decoder = DecoderSession::open_range(&path, 0, length)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let target = decoder.metadata().duration * target_fraction;
            let mut quantum = [0.0; 128 * OUTPUT_CHANNELS];
            for _ in 0..97 {
                assert_eq!(
                    decoder
                        .read_quantum(&mut quantum)
                        .unwrap_or_else(|error| panic!("{name}: live refill failed: {error}")),
                    128,
                    "{name}: ended during live refill"
                );
            }
            decoder.seek(target).unwrap_or_else(|error| {
                panic!("{name}: live seek to {target:.6}s failed: {error}")
            });
            assert_eq!(
                decoder
                    .read_quantum(&mut quantum)
                    .unwrap_or_else(|error| panic!("{name}: post-seek decode failed: {error}")),
                128,
                "{name}: live seek landed at EOF"
            );
        }
    }
}

#[test]
fn aac_decode_preserves_source_level_and_stereo_tone_identity() {
    let path = fixture("tone-aac.m4a");
    let length = path.metadata().expect("fixture metadata").len();
    let mut decoder =
        DecoderSession::open_range(&path, 0, length).expect("open AAC production decoder");
    let mut quantum = [0.0; 128 * OUTPUT_CHANNELS];
    let mut samples = Vec::new();
    while samples.len() < 52_096 * OUTPUT_CHANNELS {
        let frames = decoder
            .read_quantum(&mut quantum)
            .expect("decode AAC measurement window");
        assert!(frames > 0, "AAC ended before the measurement window");
        samples.extend_from_slice(&quantum[..frames * OUTPUT_CHANNELS]);
    }
    let window = &samples[4_096 * OUTPUT_CHANNELS..52_096 * OUTPUT_CHANNELS];
    let left = channel_samples(window, 0);
    let right = channel_samples(window, 1);
    let left_peak = peak(&left);
    let right_peak = peak(&right);
    let left_rms = rms(&left);
    let right_rms = rms(&right);
    let left_440 = tone_amplitude(&left, 440.0);
    let left_660 = tone_amplitude(&left, 660.0);
    let right_440 = tone_amplitude(&right, 440.0);
    let right_660 = tone_amplitude(&right, 660.0);
    println!(
        "AAC PCM peak=({left_peak:.6},{right_peak:.6}) rms=({left_rms:.6},{right_rms:.6}) \
         tones L440={left_440:.6} L660={left_660:.6} R440={right_440:.6} R660={right_660:.6}"
    );
    assert!((0.12..=0.20).contains(&left_peak));
    assert!((0.12..=0.20).contains(&right_peak));
    assert!((0.09..=0.13).contains(&left_rms));
    assert!((0.09..=0.13).contains(&right_rms));
    assert!(left_440 >= 0.12 && right_660 >= 0.12);
    assert!(left_660 <= left_440 * 0.1);
    assert!(right_440 <= right_660 * 0.1);
}

fn channel_samples(interleaved: &[f32], channel: usize) -> Vec<f32> {
    interleaved
        .as_chunks::<OUTPUT_CHANNELS>()
        .0
        .iter()
        .map(|frame| frame[channel])
        .collect()
}

fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0, f32::max)
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}

fn tone_amplitude(samples: &[f32], frequency: f32) -> f32 {
    let (real, imaginary) =
        samples
            .iter()
            .enumerate()
            .fold((0.0f64, 0.0f64), |(real, imaginary), (index, sample)| {
                let phase = 2.0 * std::f64::consts::PI * f64::from(frequency) * index as f64
                    / f64::from(OUTPUT_RATE);
                (
                    real + f64::from(*sample) * phase.cos(),
                    imaginary - f64::from(*sample) * phase.sin(),
                )
            });
    (2.0 * real.hypot(imaginary) / samples.len() as f64) as f32
}

#[test]
fn bounded_blob_reader_never_exposes_bytes_outside_its_slice() {
    let path = std::env::temp_dir().join(format!(
        "lite-ui-blob-range-{}-{}.bin",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    fs::write(&path, b"prefix-media-suffix").expect("fixture");
    let mut source = BoundedFile::open(&path, 7, 5).expect("bounded file");
    let mut bytes = Vec::new();
    source.read_to_end(&mut bytes).expect("range read");
    assert_eq!(bytes, b"media");
    source.seek(SeekFrom::Start(1)).expect("range seek");
    bytes.clear();
    source.read_to_end(&mut bytes).expect("seeked range read");
    assert_eq!(bytes, b"edia");
    fs::remove_file(path).expect("cleanup fixture");
}

#[test]
fn mono_is_duplicated_without_gain_change() {
    let mut output = Vec::new();
    normalize_stereo(&[0.25, -0.5], 1, &mut output);
    assert_eq!(output, [0.25, 0.25, -0.5, -0.5]);
}

#[test]
fn forty_four_kilohertz_resampler_is_bounded_and_preserves_dc() {
    let mut resampler = SincResampler::new(44_100);
    let input = vec![0.25; 4_410 * OUTPUT_CHANNELS];
    let mut output = Vec::new();
    resampler.push(&input, &mut output);
    resampler.finish(&mut output);
    let frames = output.len() / OUTPUT_CHANNELS;
    assert!((frames as isize - OUTPUT_RATE as isize / 10).abs() <= 2);
    let stable = &output[64..output.len().saturating_sub(64)];
    assert!(stable.iter().all(|sample| (*sample - 0.25).abs() < 0.002));
}

#[test]
fn coefficient_table_makes_steady_resampling_allocation_free() {
    let mut resampler = SincResampler::new(44_100);
    let input = vec![0.1; 4096 * OUTPUT_CHANNELS];
    let mut output = Vec::with_capacity(8192 * OUTPUT_CHANNELS);
    resampler.push(&input, &mut output);
    let warmed_epoch = resampler.allocation_epoch;
    output.clear();
    for _ in 0..8 {
        resampler.push(&input, &mut output);
        output.clear();
    }
    assert_eq!(resampler.allocation_epoch, warmed_epoch);
}

#[test]
fn downsampling_rejects_out_of_band_alias_energy() {
    let source_rate = 96_000;
    let mut resampler = SincResampler::new(source_rate);
    let frames = 9600;
    let input: Vec<_> = (0..frames)
        .flat_map(|frame| {
            let sample =
                (2.0 * std::f32::consts::PI * 30_000.0 * frame as f32 / source_rate as f32).sin();
            [sample, sample]
        })
        .collect();
    let mut output = Vec::new();
    resampler.push(&input, &mut output);
    resampler.finish(&mut output);
    let rms =
        (output.iter().map(|sample| sample * sample).sum::<f32>() / output.len() as f32).sqrt();
    assert!(rms < 0.03, "out-of-band RMS was {rms}");
}
