//! Standard `backdrop-filter: blur()` raster over already-painted content.

mod kernel;

use std::{
    collections::{HashMap, HashSet},
    io,
};

use super::{
    PhysicalRect, Raster,
    box_paint::{corner_inset, corner_radii},
};
use crate::style::Computed;
#[cfg(test)]
use kernel::blur_line;
use kernel::{blur_horizontal, blur_vertical};

struct CachedBackdrop {
    sample: PhysicalRect,
    radius: usize,
    input: Vec<u32>,
    blurred: Vec<u32>,
}

/// Allocation-stable scratch and retained filter results owned by one renderer.
///
/// A desktop panel change leaves the wallpaper and every existing window
/// backdrop unchanged. Keeping each node's exact input pixels avoids repeating
/// six full-area box passes for those retained layers; comparing the pixels
/// rather than a hash makes cache reuse collision-free. Without this cache,
/// panel latency grows linearly by roughly one full blur per open window.
pub(super) struct BackdropBlur {
    first: Vec<u32>,
    second: Vec<u32>,
    reduced_first: Vec<u32>,
    reduced_second: Vec<u32>,
    vertical_sums: Vec<[u32; 2]>,
    averages: Vec<u8>,
    entries: HashMap<u64, CachedBackdrop>,
    active: HashSet<u64>,
    #[cfg(test)]
    cache_hits: usize,
}

impl BackdropBlur {
    pub(super) fn new() -> Self {
        Self {
            first: Vec::new(),
            second: Vec::new(),
            reduced_first: Vec::new(),
            reduced_second: Vec::new(),
            vertical_sums: Vec::new(),
            averages: Vec::new(),
            entries: HashMap::new(),
            active: HashSet::new(),
            #[cfg(test)]
            cache_hits: 0,
        }
    }

    pub(super) fn begin_frame(&mut self) {
        self.active.clear();
    }

    pub(super) fn finish_frame(&mut self) {
        self.entries
            .retain(|node_id, _| self.active.contains(node_id));
    }

    /// Applies the CSS blur to one element's backdrop, reusing an unchanged
    /// retained filter layer when its exact input pixels still match.
    pub(super) fn paint<R: Raster>(
        &mut self,
        target: &mut R,
        node_id: u64,
        bounds: PhysicalRect,
        clip: Option<PhysicalRect>,
        computed: &Computed,
    ) -> io::Result<()> {
        let Some(radius) = blur_radius(computed)? else {
            return Ok(());
        };
        if radius == 0 || bounds.x2 <= bounds.x1 || bounds.y2 <= bounds.y1 {
            return Ok(());
        }
        self.active.insert(node_id);
        // Three equal box passes approximate the Gaussian standard deviation
        // required by CSS blur while every pass remains O(area). The 3r support
        // captures the complete kernel instead of clamping at the element edge.
        let support = radius.saturating_mul(3);
        let sample = PhysicalRect {
            x1: bounds.x1.saturating_sub(support),
            y1: bounds.y1.saturating_sub(support),
            x2: bounds.x2.saturating_add(support).min(target.width()),
            y2: bounds.y2.saturating_add(support).min(target.height()),
        };
        let width = sample.x2 - sample.x1;
        let height = sample.y2 - sample.y1;
        if let Some(cached) = self.entries.get(&node_id).filter(|cached| {
            cached.sample == sample
                && cached.radius == radius
                && (0..height).all(|y| {
                    cached.input[y * width..(y + 1) * width]
                        == target.row(sample.y1 + y)[sample.x1..sample.x2]
                })
        }) {
            #[cfg(test)]
            {
                self.cache_hits += 1;
            }
            blit(target, bounds, clip, computed, sample, &cached.blurred);
            return Ok(());
        }
        let mut input = self
            .entries
            .get_mut(&node_id)
            .map(|cached| std::mem::take(&mut cached.input))
            .unwrap_or_default();
        input.resize(width * height, 0);
        for y in 0..height {
            input[y * width..(y + 1) * width]
                .copy_from_slice(&target.row(sample.y1 + y)[sample.x1..sample.x2]);
        }
        self.first.resize(width * height, 0);
        let scale = blur_scale(radius);
        let (blur_width, blur_height, blur_radius) = if scale == 1 {
            self.first.copy_from_slice(&input);
            (width, height, radius)
        } else {
            let reduced_width = width.div_ceil(scale);
            let reduced_height = height.div_ceil(scale);
            self.reduced_first.resize(reduced_width * reduced_height, 0);
            self.reduced_second
                .resize(reduced_width * reduced_height, 0);
            downsample(&input, &mut self.reduced_first, width, height, scale);
            (reduced_width, reduced_height, radius.div_ceil(scale))
        };
        let cached = self
            .entries
            .entry(node_id)
            .or_insert_with(|| CachedBackdrop {
                sample,
                radius,
                input: Vec::new(),
                blurred: Vec::new(),
            });
        cached.sample = sample;
        cached.radius = radius;
        cached.input = input;
        self.vertical_sums.resize(blur_width, [0; 2]);
        let diameter = blur_radius * 2 + 1;
        let maximum_sum = diameter * 255;
        self.averages.resize(maximum_sum + 1, 0);
        for (sum, average) in self.averages.iter_mut().enumerate() {
            *average = ((sum + diameter / 2) / diameter) as u8;
        }
        if scale == 1 {
            self.second.resize(width * height, 0);
            for _ in 0..3 {
                blur_horizontal(
                    &self.first,
                    &mut self.second,
                    width,
                    height,
                    blur_radius,
                    &self.averages,
                );
                blur_vertical(
                    &self.second,
                    &mut self.first,
                    width,
                    height,
                    blur_radius,
                    &mut self.vertical_sums,
                    &self.averages,
                );
            }
        } else {
            for _ in 0..3 {
                blur_horizontal(
                    &self.reduced_first,
                    &mut self.reduced_second,
                    blur_width,
                    blur_height,
                    blur_radius,
                    &self.averages,
                );
                blur_vertical(
                    &self.reduced_second,
                    &mut self.reduced_first,
                    blur_width,
                    blur_height,
                    blur_radius,
                    &mut self.vertical_sums,
                    &self.averages,
                );
            }
            upsample(
                &self.reduced_first,
                &mut self.first,
                &mut self.reduced_second,
                blur_width,
                blur_height,
                width,
                height,
                scale,
            );
        }
        std::mem::swap(&mut cached.blurred, &mut self.first);
        blit(target, bounds, clip, computed, sample, &cached.blurred);
        Ok(())
    }
}

fn blit<R: Raster>(
    target: &mut R,
    bounds: PhysicalRect,
    clip: Option<PhysicalRect>,
    computed: &Computed,
    sample: PhysicalRect,
    blurred: &[u32],
) {
    let output = clip.map_or(bounds, |clip| bounds.intersect(clip));
    if output.x2 <= output.x1 || output.y2 <= output.y1 {
        return;
    }
    let width = sample.x2 - sample.x1;
    let radii = corner_radii(computed);
    let box_height = bounds.y2 - bounds.y1;
    for y in output.y1..output.y2 {
        let row_y = y - bounds.y1;
        let left = corner_inset(radii[0], radii[3], row_y, box_height);
        let right = corner_inset(radii[1], radii[2], row_y, box_height);
        let x1 = (bounds.x1 as f32 + left).ceil() as usize;
        let x2 = (bounds.x2 as f32 - right).floor().max(x1 as f32) as usize;
        let x1 = x1.max(output.x1);
        let x2 = x2.min(output.x2);
        if x2 > x1 {
            let source_y = y - sample.y1;
            let source_x = x1 - sample.x1;
            target.row_mut(y)[x1..x2].copy_from_slice(
                &blurred[source_y * width + source_x..source_y * width + source_x + (x2 - x1)],
            );
        }
    }
}

fn blur_radius(computed: &Computed) -> io::Result<Option<usize>> {
    let Some(value) = computed.get("backdrop-filter") else {
        return Ok(None);
    };
    let value = value.trim();
    if value == "none" {
        return Ok(None);
    }
    let radius = value
        .strip_prefix("blur(")
        .and_then(|value| value.strip_suffix(')'))
        .and_then(|value| value.trim().strip_suffix("px"))
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported backdrop-filter '{value}'"),
            )
        })?;
    Ok(Some(
        (radius * super::SCALE).round().clamp(0.0, 128.0) as usize
    ))
}

fn blur_scale(radius: usize) -> usize {
    if radius >= 16 { 4 } else { 1 }
}

fn downsample(source: &[u32], target: &mut [u32], width: usize, height: usize, scale: usize) {
    let target_width = width.div_ceil(scale);
    let target_height = height.div_ceil(scale);
    for target_y in 0..target_height {
        let y1 = target_y * scale;
        let y2 = (y1 + scale).min(height);
        for target_x in 0..target_width {
            let x1 = target_x * scale;
            let x2 = (x1 + scale).min(width);
            let mut sums = [0u32; 2];
            for y in y1..y2 {
                for &pixel in &source[y * width + x1..y * width + x2] {
                    sums[0] += pixel & 0x00ff_00ff;
                    sums[1] += (pixel >> 8) & 0x00ff_00ff;
                }
            }
            let count = ((x2 - x1) * (y2 - y1)) as u32;
            let rb = sums[0];
            let ag = sums[1];
            target[target_y * target_width + target_x] = ((rb & 0xffff) + count / 2) / count
                | ((((ag & 0xffff) + count / 2) / count) << 8)
                | ((((rb >> 16) + count / 2) / count) << 16)
                | ((((ag >> 16) + count / 2) / count) << 24);
        }
    }
}

fn upsample(
    source: &[u32],
    target: &mut [u32],
    horizontal: &mut Vec<u32>,
    source_width: usize,
    source_height: usize,
    width: usize,
    height: usize,
    scale: usize,
) {
    horizontal.resize(width * source_height, 0);
    // Bilinear reconstruction is separable: expanding each source row once
    // avoids repeating both horizontal interpolations for every output row.
    for source_y in 0..source_height {
        for x in 0..width {
            let source_x = x / scale;
            let next_x = (source_x + 1).min(source_width - 1);
            let weight_x = x % scale;
            horizontal[source_y * width + x] = lerp_pixel(
                source[source_y * source_width + source_x],
                source[source_y * source_width + next_x],
                weight_x,
                scale,
            );
        }
    }
    for y in 0..height {
        let source_y = y / scale;
        let next_y = (source_y + 1).min(source_height - 1);
        let weight_y = y % scale;
        for x in 0..width {
            target[y * width + x] = lerp_pixel(
                horizontal[source_y * width + x],
                horizontal[next_y * width + x],
                weight_y,
                scale,
            );
        }
    }
}

fn lerp_pixel(first: u32, second: u32, weight: usize, scale: usize) -> u32 {
    if weight == 0 || first == second {
        return first;
    }
    let first_rb = (u64::from(first & 0x00ff_0000) << 16) | u64::from(first & 0xff);
    let second_rb = (u64::from(second & 0x00ff_0000) << 16) | u64::from(second & 0xff);
    let first_ag = (u64::from(first & 0xff00_0000) << 8) | u64::from((first >> 8) & 0xff);
    let second_ag = (u64::from(second & 0xff00_0000) << 8) | u64::from((second >> 8) & 0xff);
    let weights = (scale - weight, weight);
    let rounding = (u64::try_from(scale / 2).expect("small scale") << 32)
        | u64::try_from(scale / 2).expect("small scale");
    let rb = first_rb * weights.0 as u64 + second_rb * weights.1 as u64 + rounding;
    let ag = first_ag * weights.0 as u64 + second_ag * weights.1 as u64 + rounding;
    let divisor = scale as u64;
    (((rb & 0xffff_ffff) / divisor) as u32 & 0xff)
        | ((((ag & 0xffff_ffff) / divisor) as u32 & 0xff) << 8)
        | ((((rb >> 32) / divisor) as u32 & 0xff) << 16)
        | ((((ag >> 32) / divisor) as u32 & 0xff) << 24)
}

#[cfg(test)]
mod tests {
    use super::{
        BackdropBlur, PhysicalRect, blur_line, blur_scale, blur_vertical, downsample, lerp_pixel,
        upsample,
    };
    use crate::{renderer::Raster, style::Computed};

    struct TestRaster {
        pixels: Vec<u32>,
        width: usize,
        height: usize,
    }

    impl Raster for TestRaster {
        fn width(&self) -> usize {
            self.width
        }

        fn height(&self) -> usize {
            self.height
        }

        fn row(&self, row: usize) -> &[u32] {
            &self.pixels[row * self.width..(row + 1) * self.width]
        }

        fn row_mut(&mut self, row: usize) -> &mut [u32] {
            &mut self.pixels[row * self.width..(row + 1) * self.width]
        }
    }

    #[test]
    fn box_blur_is_symmetric_and_preserves_opaque_alpha() {
        let source = [0xff00_0000, 0xffff_ffff, 0xff00_0000];
        let mut target = [0; 3];
        let averages: Vec<u8> = (0..=765).map(|sum| ((sum + 1) / 3) as u8).collect();
        blur_line(&source, &mut target, 1, &averages);
        assert_eq!(target[0], target[2]);
        assert!(target.iter().all(|pixel| pixel >> 24 == 0xff));
        assert_eq!(target[1] & 0xff, 85);
    }

    #[test]
    fn row_major_vertical_pass_matches_the_same_box_kernel_per_column() {
        let width = 5;
        let height = 4;
        let source: Vec<u32> = (0..width * height)
            .map(|value| 0xff00_0000 | (value as u32 * 0x0001_0101))
            .collect();
        let mut actual = vec![0; source.len()];
        let averages: Vec<u8> = (0..=765).map(|sum| ((sum + 1) / 3) as u8).collect();
        let mut sums = vec![[0; 2]; width];
        blur_vertical(&source, &mut actual, width, height, 1, &mut sums, &averages);

        let mut expected = vec![0; source.len()];
        for x in 0..width {
            let column: Vec<u32> = (0..height).map(|y| source[y * width + x]).collect();
            let mut blurred = vec![0; height];
            blur_line(&column, &mut blurred, 1, &averages);
            for y in 0..height {
                expected[y * width + x] = blurred[y];
            }
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn retained_blur_reuses_only_an_exact_backdrop_snapshot() {
        let mut target = TestRaster {
            pixels: vec![0xff10_2030; 12 * 12],
            width: 12,
            height: 12,
        };
        let original = target.pixels.clone();
        let mut computed = Computed::default();
        computed.set("backdrop-filter", "blur(1px)");
        let bounds = PhysicalRect {
            x1: 2,
            y1: 2,
            x2: 10,
            y2: 10,
        };
        let mut blur = BackdropBlur::new();
        blur.begin_frame();
        blur.paint(&mut target, 7, bounds, None, &computed)
            .expect("first blur");
        blur.finish_frame();
        let expected = target.pixels.clone();

        target.pixels.clone_from(&original);
        blur.begin_frame();
        blur.paint(&mut target, 7, bounds, None, &computed)
            .expect("cached blur");
        blur.finish_frame();
        assert_eq!(target.pixels, expected);
        assert_eq!(blur.cache_hits, 1);

        target.pixels.clone_from(&original);
        target.pixels[0] = 0xffff_ffff;
        blur.begin_frame();
        blur.paint(&mut target, 7, bounds, None, &computed)
            .expect("changed blur");
        blur.finish_frame();
        assert_eq!(blur.cache_hits, 1);
    }

    #[test]
    fn multiresolution_blur_preserves_a_uniform_backdrop_bit_exactly() {
        assert_eq!(blur_scale(15), 1);
        assert_eq!(blur_scale(16), 4);
        let source = vec![0xff12_3456; 17 * 13];
        let mut reduced = vec![0; 5 * 4];
        downsample(&source, &mut reduced, 17, 13, 4);
        assert!(reduced.iter().all(|pixel| *pixel == 0xff12_3456));

        let mut restored = vec![0; source.len()];
        let mut horizontal = Vec::new();
        upsample(&reduced, &mut restored, &mut horizontal, 5, 4, 17, 13, 4);
        assert_eq!(restored, source);
    }

    #[test]
    fn packed_bilinear_channels_match_scalar_rounding() {
        let first = 0x1234_5678;
        let second = 0xfedc_ba98;
        let expected = [24, 16, 8, 0].into_iter().fold(0, |pixel, shift| {
            let first = (first >> shift) & 0xff;
            let second = (second >> shift) & 0xff;
            pixel | ((first * 3 + second + 2) / 4) << shift
        });
        assert_eq!(lerp_pixel(first, second, 1, 4), expected);
    }
}
