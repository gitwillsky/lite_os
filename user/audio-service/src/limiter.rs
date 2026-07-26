use audio_proto::CHANNELS;

pub(crate) const LOOKAHEAD_FRAMES: usize = 128;
// exp(-128 / 2400): one conventional 50 ms release time constant.
const RELEASE_REMAINDER: f32 = 0.948_063_9;

/// Fixed-lookahead overload protection for the final system mix.
pub(crate) struct Limiter {
    delayed: [[f32; CHANNELS]; LOOKAHEAD_FRAMES],
    gain: f32,
    activations: u64,
    maximum_reduction: f32,
}

impl Limiter {
    pub(crate) const fn new() -> Self {
        Self {
            delayed: [[0.0; CHANNELS]; LOOKAHEAD_FRAMES],
            gain: 1.0,
            activations: 0,
            maximum_reduction: 0.0,
        }
    }

    /// Emits the previous block and holds `input` for exactly 128-frame lookahead.
    pub(crate) fn process(
        &mut self,
        input: &[[f32; CHANNELS]; LOOKAHEAD_FRAMES],
        output: &mut [[f32; CHANNELS]; LOOKAHEAD_FRAMES],
    ) {
        output.copy_from_slice(&self.delayed);
        self.delayed.copy_from_slice(input);

        let peak = input
            .iter()
            .flat_map(|frame| frame.iter())
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        let ceiling = if peak > 1.0 { 1.0 / peak } else { 1.0 };
        let released = 1.0 - (1.0 - self.gain) * RELEASE_REMAINDER;
        let next_gain = released.min(ceiling);
        if ceiling < 1.0 && next_gain < self.gain {
            self.activations = self.activations.saturating_add(1);
        }
        self.maximum_reduction = self.maximum_reduction.max(1.0 - next_gain);
        self.gain = next_gain;

        for frame in &mut self.delayed {
            for sample in frame {
                *sample *= next_gain;
            }
        }
    }

    pub(crate) const fn activations(&self) -> u64 {
        self.activations
    }

    pub(crate) const fn maximum_reduction(&self) -> f32 {
        self.maximum_reduction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_below_ceiling_after_fixed_lookahead() {
        let mut limiter = Limiter::new();
        let input = [[0.75, -0.5]; LOOKAHEAD_FRAMES];
        let mut output = [[1.0; CHANNELS]; LOOKAHEAD_FRAMES];
        limiter.process(&input, &mut output);
        assert_eq!(output, [[0.0; CHANNELS]; LOOKAHEAD_FRAMES]);
        limiter.process(&[[0.0; CHANNELS]; LOOKAHEAD_FRAMES], &mut output);
        assert_eq!(output, input);
        assert_eq!(limiter.activations(), 0);
        assert_eq!(limiter.maximum_reduction(), 0.0);
    }

    #[test]
    fn overload_never_crosses_brick_wall_and_releases_in_fifty_ms() {
        let mut limiter = Limiter::new();
        let hot = [[2.0, -1.5]; LOOKAHEAD_FRAMES];
        let mut output = [[0.0; CHANNELS]; LOOKAHEAD_FRAMES];
        limiter.process(&hot, &mut output);
        limiter.process(&[[0.0; CHANNELS]; LOOKAHEAD_FRAMES], &mut output);
        assert!(output.iter().flatten().all(|sample| sample.abs() <= 1.0));
        assert_eq!(limiter.maximum_reduction(), 0.5);

        for _ in 0..18 {
            limiter.process(&[[0.0; CHANNELS]; LOOKAHEAD_FRAMES], &mut output);
        }
        let expected = 1.0 - 0.5 * RELEASE_REMAINDER.powi(19);
        assert!((limiter.gain - expected).abs() < f32::EPSILON * 4.0);
    }
}
