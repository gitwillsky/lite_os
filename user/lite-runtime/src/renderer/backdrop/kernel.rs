//! Allocation-free separable box blur kernel.

pub(super) fn blur_horizontal(
    source: &[u32],
    target: &mut [u32],
    width: usize,
    height: usize,
    radius: usize,
    averages: &[u8],
) {
    for y in 0..height {
        blur_line(
            &source[y * width..(y + 1) * width],
            &mut target[y * width..(y + 1) * width],
            radius,
            averages,
        );
    }
}

pub(super) fn blur_vertical(
    source: &[u32],
    target: &mut [u32],
    width: usize,
    height: usize,
    radius: usize,
    sums: &mut [[u32; 2]],
    averages: &[u8],
) {
    if height == 0 {
        return;
    }
    debug_assert_eq!(sums.len(), width);
    sums.fill([0; 2]);
    let last = height - 1;
    let diameter = radius * 2 + 1;
    // 1. Build every column's first sliding sum by scanning complete source
    // rows. The previous x-major walk jumped `width` pixels for every load,
    // thrashing cache on a full-screen blur.
    for offset in 0..diameter {
        let row = offset.saturating_sub(radius).min(last);
        for (sum, pixel) in sums.iter_mut().zip(&source[row * width..(row + 1) * width]) {
            add(sum, *pixel);
        }
    }
    // 2. Emit and advance one complete destination row at a time. Source,
    // destination and the reused sum plane are now all contiguous.
    for y in 0..height {
        for (pixel, sum) in target[y * width..(y + 1) * width]
            .iter_mut()
            .zip(sums.iter())
        {
            *pixel = average(*sum, averages);
        }
        if y + 1 == height {
            continue;
        }
        let remove = y.saturating_sub(radius).min(last);
        let insert = y.saturating_add(radius + 1).min(last);
        for ((sum, old), new) in sums
            .iter_mut()
            .zip(&source[remove * width..(remove + 1) * width])
            .zip(&source[insert * width..(insert + 1) * width])
        {
            subtract(sum, *old);
            add(sum, *new);
        }
    }
}

pub(super) fn blur_line(source: &[u32], target: &mut [u32], radius: usize, averages: &[u8]) {
    if source.is_empty() {
        return;
    }
    let last = source.len() - 1;
    let diameter = radius * 2 + 1;
    let mut sums = [0u32; 2];
    for offset in 0..diameter {
        add(&mut sums, source[offset.saturating_sub(radius).min(last)]);
    }
    for (index, pixel) in target.iter_mut().enumerate() {
        *pixel = average(sums, averages);
        if index + 1 < source.len() {
            subtract(&mut sums, source[index.saturating_sub(radius).min(last)]);
            add(
                &mut sums,
                source[index.saturating_add(radius + 1).min(last)],
            );
        }
    }
}

fn add(sums: &mut [u32; 2], pixel: u32) {
    // Two 16-bit lanes fit the maximum 257×255 channel sum exactly. Packing
    // R/B and A/G halves the hot-loop arithmetic while preserving every
    // rounded channel bit produced by the scalar CSS box kernel.
    sums[0] += pixel & 0x00ff_00ff;
    sums[1] += (pixel >> 8) & 0x00ff_00ff;
}

fn subtract(sums: &mut [u32; 2], pixel: u32) {
    sums[0] -= pixel & 0x00ff_00ff;
    sums[1] -= (pixel >> 8) & 0x00ff_00ff;
}

fn average(sums: [u32; 2], averages: &[u8]) -> u32 {
    let rb = sums[0];
    let ag = sums[1];
    u32::from(averages[(rb & 0xffff) as usize])
        | (u32::from(averages[(ag & 0xffff) as usize]) << 8)
        | (u32::from(averages[(rb >> 16) as usize]) << 16)
        | (u32::from(averages[(ag >> 16) as usize]) << 24)
}
