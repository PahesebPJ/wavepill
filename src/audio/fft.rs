//! Pure FFT spectrum analyzer with Hann windowing, logarithmic frequency bands,
//! normalized dB scaling, asymmetric exponential smoothing, and RMS hysteresis.

use std::f32::consts::PI;
use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex};

use super::capture::SpectrumFrame;

/// Target audio sample rate in Hz.
pub const SAMPLE_RATE: u32 = 44_100;
/// FFT window size in samples.
pub const FFT_SIZE: usize = 1024;

/// Lower frequency bound for logarithmic spectrum bands (Hz).
pub const MIN_FREQ_HZ: f32 = 60.0;
/// Upper frequency bound for logarithmic spectrum bands (Hz).
pub const MAX_FREQ_HZ: f32 = 14_000.0;

/// Minimum dB level mapped to `0.0`.
pub const MIN_DB: f32 = -60.0;
/// Maximum dB level mapped to `1.0`.
pub const MAX_DB: f32 = 0.0;

/// Fast exponential attack smoothing factor.
const ATTACK_ALPHA: f32 = 0.65;
/// Gentle gravitational decay smoothing factor.
const DECAY_ALPHA: f32 = 0.18;
/// Snap-to-zero threshold once a bar has decayed near silence.
const ZERO_SNAP_EPS: f32 = 1e-4;

/// RMS threshold required to transition from silent to audible.
pub const AUDIBLE_ON_RMS: f32 = 0.003;
/// RMS threshold below which an audible stream transitions back to silent.
pub const AUDIBLE_OFF_RMS: f32 = 0.001;

/// Pure DSP analyzer converting mono `f32` PCM frames into normalized spectrum bars.
pub struct SpectrumAnalyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    energy_norm: f32,
    band_bins: Vec<(usize, usize)>,
    fft_buffer: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    smoothed_bars: Vec<f32>,
    was_audible: bool,
}

impl SpectrumAnalyzer {
    /// Creates a new spectrum analyzer configured for `bar_count` logarithmic bands.
    pub fn new(bar_count: usize) -> Self {
        let bar_count = bar_count.max(1);

        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let scratch_len = fft.get_inplace_scratch_len();

        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|n| 0.5 * (1.0 - ((2.0 * PI * n as f32) / (FFT_SIZE - 1) as f32).cos()))
            .collect();

        // Parseval normalization for real signals: half the energy sits in positive frequencies.
        let window_energy: f32 = window.iter().map(|w| w * w).sum();
        let energy_norm = ((FFT_SIZE as f32 / 4.0) * window_energy).sqrt().max(1e-6);

        let band_bins = compute_band_bins(bar_count);

        Self {
            fft,
            window,
            energy_norm,
            band_bins,
            fft_buffer: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            scratch: vec![Complex::new(0.0, 0.0); scratch_len],
            smoothed_bars: vec![0.0; bar_count],
            was_audible: false,
        }
    }

    /// Returns the configured number of spectrum bars.
    pub fn bar_count(&self) -> usize {
        self.smoothed_bars.len()
    }

    /// Resets smoothed bars and audibility hysteresis state to silence.
    pub fn reset(&mut self) {
        self.smoothed_bars.fill(0.0);
        self.was_audible = false;
    }

    /// Processes a chunk of mono `f32` audio samples and produces a smoothed [`SpectrumFrame`].
    pub fn process(&mut self, samples: &[f32]) -> SpectrumFrame {
        let active_samples = if samples.len() > FFT_SIZE {
            &samples[samples.len() - FFT_SIZE..]
        } else {
            samples
        };

        let rms = compute_rms(active_samples);
        self.was_audible = if self.was_audible {
            rms >= AUDIBLE_OFF_RMS
        } else {
            rms >= AUDIBLE_ON_RMS
        };

        for (i, slot) in self.fft_buffer.iter_mut().enumerate() {
            let sample = active_samples
                .get(i)
                .copied()
                .filter(|s| s.is_finite())
                .unwrap_or(0.0);
            *slot = Complex::new(sample * self.window[i], 0.0);
        }

        self.fft
            .process_with_scratch(&mut self.fft_buffer, &mut self.scratch);

        for (band_idx, &(start_bin, end_bin)) in self.band_bins.iter().enumerate() {
            let power_sum: f32 = self.fft_buffer[start_bin..end_bin]
                .iter()
                .map(|c| c.norm_sqr())
                .sum();

            let amplitude = power_sum.sqrt() / self.energy_norm;
            let db = if amplitude > 1e-12 {
                20.0 * amplitude.log10()
            } else {
                MIN_DB
            };
            let target = ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0);

            let current = self.smoothed_bars[band_idx];
            let alpha = if target > current {
                ATTACK_ALPHA
            } else {
                DECAY_ALPHA
            };
            let mut next = current + alpha * (target - current);
            if target == 0.0 && next < ZERO_SNAP_EPS {
                next = 0.0;
            }
            self.smoothed_bars[band_idx] = next.clamp(0.0, 1.0);
        }

        SpectrumFrame {
            bars: self.smoothed_bars.clone(),
            rms,
            is_audible: self.was_audible,
        }
    }
}

fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    let sum_sq: f32 = samples
        .iter()
        .copied()
        .map(|s| if s.is_finite() { s * s } else { 0.0 })
        .sum();

    (sum_sq / samples.len() as f32).sqrt()
}

fn compute_band_bins(bar_count: usize) -> Vec<(usize, usize)> {
    let nyquist_bin = FFT_SIZE / 2;
    let bin_hz = SAMPLE_RATE as f32 / FFT_SIZE as f32;
    let ratio = MAX_FREQ_HZ / MIN_FREQ_HZ;

    (0..bar_count)
        .map(|i| {
            let f_low = MIN_FREQ_HZ * ratio.powf(i as f32 / bar_count as f32);
            let f_high = MIN_FREQ_HZ * ratio.powf((i + 1) as f32 / bar_count as f32);

            let start = ((f_low / bin_hz).floor() as usize).clamp(1, nyquist_bin - 1);
            let end = ((f_high / bin_hz).ceil() as usize).clamp(start + 1, nyquist_bin);
            (start, end)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_wave(freq_hz: f32, amplitude: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|n| amplitude * (2.0 * PI * freq_hz * n as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    fn peak_bar_index(bars: &[f32]) -> usize {
        bars.iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(idx, _)| idx)
            .unwrap()
    }

    #[test]
    fn silence_produces_zero_bars_and_not_audible() {
        let mut analyzer = SpectrumAnalyzer::new(12);
        let frame = analyzer.process(&vec![0.0; FFT_SIZE]);

        assert_eq!(frame.bars.len(), 12);
        assert!(frame.bars.iter().all(|&b| b == 0.0));
        assert_eq!(frame.rms, 0.0);
        assert!(!frame.is_audible);
    }

    #[test]
    fn sine_waves_peak_in_expected_logarithmic_bands() {
        let mut low_mid_analyzer = SpectrumAnalyzer::new(12);
        let low_mid_samples = sine_wave(440.0, 0.8, FFT_SIZE);
        let low_mid_frame = low_mid_analyzer.process(&low_mid_samples);

        assert!(low_mid_frame.is_audible);
        assert!(
            (low_mid_frame.rms - (0.8 / 2.0_f32.sqrt())).abs() < 0.02,
            "unexpected rms: {}",
            low_mid_frame.rms
        );
        assert!(
            low_mid_frame
                .bars
                .iter()
                .all(|&b| b.is_finite() && (0.0..=1.0).contains(&b))
        );

        let low_peak = peak_bar_index(&low_mid_frame.bars);

        let mut high_analyzer = SpectrumAnalyzer::new(12);
        let high_samples = sine_wave(8_000.0, 0.8, FFT_SIZE);
        let high_frame = high_analyzer.process(&high_samples);
        let high_peak = peak_bar_index(&high_frame.bars);

        // 440 Hz sits in the lower-mid bars (~4), 8 kHz sits near the top bars (~10).
        assert!(
            (3..=5).contains(&low_peak),
            "440 Hz peaked at unexpected bar {low_peak}: {:?}",
            low_mid_frame.bars
        );
        assert!(
            high_peak >= 9,
            "8 kHz peaked at unexpected bar {high_peak}: {:?}",
            high_frame.bars
        );
        assert!(low_mid_frame.bars[low_peak] > 0.5);
        assert!(high_frame.bars[high_peak] > 0.5);
        assert!(low_mid_frame.bars[11] < 0.05);
        assert!(high_frame.bars[0] < 0.05);
    }

    #[test]
    fn smoothing_has_fast_attack_and_gradual_decay() {
        let mut analyzer = SpectrumAnalyzer::new(12);
        let tone = sine_wave(1_000.0, 0.9, FFT_SIZE);

        let f1 = analyzer.process(&tone);
        let peak_idx = peak_bar_index(&f1.bars);
        assert!(f1.bars[peak_idx] > 0.5);

        let f2 = analyzer.process(&tone);
        assert!(f2.bars[peak_idx] >= f1.bars[peak_idx]);

        let silence = vec![0.0; FFT_SIZE];
        let d1 = analyzer.process(&silence);
        let d2 = analyzer.process(&silence);

        assert!(d1.bars[peak_idx] < f2.bars[peak_idx]);
        assert!(d1.bars[peak_idx] > 0.4);
        assert!(d2.bars[peak_idx] < d1.bars[peak_idx]);
        assert!(d2.bars[peak_idx] > 0.3);

        // Eventually settles cleanly to zero after prolonged silence.
        let mut last = d2;
        for _ in 0..80 {
            last = analyzer.process(&silence);
        }
        assert_eq!(last.bars[peak_idx], 0.0);
    }

    #[test]
    fn audibility_hysteresis_prevents_flicker() {
        let mut analyzer = SpectrumAnalyzer::new(12);

        // Deadband amplitude between AUDIBLE_OFF_RMS (0.001) and AUDIBLE_ON_RMS (0.003).
        let deadband_samples = vec![0.002; FFT_SIZE];
        let f1 = analyzer.process(&deadband_samples);
        assert!(!f1.is_audible, "should stay silent until ON threshold");

        // Cross ON threshold.
        let loud_samples = vec![0.05; FFT_SIZE];
        let f2 = analyzer.process(&loud_samples);
        assert!(f2.is_audible, "should trigger audible above ON threshold");

        // Drop back into deadband: hysteresis keeps it audible.
        let f3 = analyzer.process(&deadband_samples);
        assert!(f3.is_audible, "hysteresis should hold audible inside band");

        // Drop below OFF threshold: becomes silent.
        let quiet_samples = vec![0.0005; FFT_SIZE];
        let f4 = analyzer.process(&quiet_samples);
        assert!(!f4.is_audible, "should turn silent below OFF threshold");
    }

    #[test]
    fn handles_arbitrary_slice_lengths_and_non_finite_samples() {
        let mut analyzer = SpectrumAnalyzer::new(12);

        let empty = analyzer.process(&[]);
        assert_eq!(empty.bars.len(), 12);
        assert_eq!(empty.rms, 0.0);
        assert!(!empty.is_audible);

        let short = analyzer.process(&[0.25; 128]);
        assert_eq!(short.bars.len(), 12);
        assert!(short.bars.iter().all(|b| b.is_finite()));

        let long = analyzer.process(&sine_wave(500.0, 0.5, 2500));
        assert_eq!(long.bars.len(), 12);
        assert!(long.is_audible);

        let corrupted = analyzer.process(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.1]);
        assert!(corrupted.rms.is_finite());
        assert!(
            corrupted
                .bars
                .iter()
                .all(|&b| b.is_finite() && (0.0..=1.0).contains(&b))
        );
    }

    #[test]
    fn supports_min_max_and_clamped_bar_counts() {
        for count in [0, 4, 12, 32] {
            let mut analyzer = SpectrumAnalyzer::new(count);
            let frame = analyzer.process(&sine_wave(440.0, 0.5, FFT_SIZE));
            assert_eq!(frame.bars.len(), count.max(1));
            assert!(
                frame
                    .bars
                    .iter()
                    .all(|&b| b.is_finite() && (0.0..=1.0).contains(&b))
            );
        }
    }
}
