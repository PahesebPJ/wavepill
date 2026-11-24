//! PulseAudio/PipeWire monitor capture engine running on a dedicated OS thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use libpulse_binding as pulse;
use libpulse_simple_binding as psimple;
use psimple::Simple;
use pulse::def::BufferAttr;
use pulse::sample::{Format, Spec};
use pulse::stream::Direction;
use tokio::sync::watch;

use super::fft::{FFT_SIZE, SAMPLE_RATE, SpectrumAnalyzer};

const MONITOR_DEVICE: &str = "@DEFAULT_MONITOR@";
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(2);
const STOP_JOIN_TIMEOUT: Duration = Duration::from_millis(150);

/// Processed spectrum data emitted by [`AudioEngine`].
#[derive(Debug, Clone, PartialEq)]
pub struct SpectrumFrame {
    pub bars: Vec<f32>,
    pub rms: f32,
    pub is_audible: bool,
}

impl SpectrumFrame {
    /// Creates an all-zero silent frame for the given bar count.
    pub fn silent(bar_count: usize) -> Self {
        Self {
            bars: vec![0.0; bar_count.max(1)],
            rms: 0.0,
            is_audible: false,
        }
    }
}

/// Background audio capture engine streaming `@DEFAULT_MONITOR@` through [`SpectrumAnalyzer`].
pub struct AudioEngine {
    stop_flag: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    rx: watch::Receiver<SpectrumFrame>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl AudioEngine {
    /// Starts capturing system audio on a background thread and publishes
    /// processed [`SpectrumFrame`] updates over a `watch` channel.
    pub fn start(bar_count: usize) -> Self {
        let bar_count = bar_count.max(1);
        let (tx, rx) = watch::channel(SpectrumFrame::silent(bar_count));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));

        let thread_stop = Arc::clone(&stop_flag);
        let thread_wake = Arc::clone(&wake);

        let handle = thread::Builder::new()
            .name("wavepill-audio".to_owned())
            .spawn(move || {
                capture_loop(bar_count, tx, thread_stop, thread_wake);
            })
            .expect("failed to spawn wavepill-audio thread");

        Self {
            stop_flag,
            wake,
            rx,
            handle: Mutex::new(Some(handle)),
        }
    }

    /// Returns a receiver that yields the latest [`SpectrumFrame`].
    pub fn subscribe(&self) -> watch::Receiver<SpectrumFrame> {
        self.rx.clone()
    }

    /// Returns a snapshot of the most recent [`SpectrumFrame`].
    pub fn latest(&self) -> SpectrumFrame {
        self.rx.borrow().clone()
    }

    /// Signals the capture thread to stop and waits briefly for clean exit.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        let (_lock, cvar) = &*self.wake;
        cvar.notify_all();

        let Ok(mut guard) = self.handle.lock() else {
            return;
        };
        let Some(handle) = guard.take() else {
            return;
        };

        // `pa_simple_read` can block indefinitely if PipeWire suspends the idle sink monitor,
        // so only block on `join()` when the thread exits within the grace window.
        let deadline = Instant::now() + STOP_JOIN_TIMEOUT;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }

        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_loop(
    bar_count: usize,
    tx: watch::Sender<SpectrumFrame>,
    stop_flag: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
) {
    let mut analyzer = SpectrumAnalyzer::new(bar_count);
    let mut raw_bytes = vec![0u8; FFT_SIZE * std::mem::size_of::<f32>()];
    let mut samples = vec![0.0_f32; FFT_SIZE];
    let mut backoff = INITIAL_BACKOFF;

    while !stop_flag.load(Ordering::Acquire) {
        let stream = match open_monitor_stream() {
            Ok(stream) => {
                tracing::info!(
                    device = MONITOR_DEVICE,
                    "Connected PulseAudio monitor stream"
                );
                backoff = INITIAL_BACKOFF;
                stream
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    retry_ms = backoff.as_millis(),
                    "Failed to open PulseAudio monitor stream; retrying"
                );
                publish_silent(&tx, &mut analyzer, bar_count);
                wait_interruptible(&stop_flag, &wake, backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        while !stop_flag.load(Ordering::Acquire) {
            if let Err(err) = stream.read(&mut raw_bytes) {
                tracing::warn!(error = %err, "PulseAudio monitor read failed; reconnecting");
                publish_silent(&tx, &mut analyzer, bar_count);
                break;
            }

            let (chunks, _) = raw_bytes.as_chunks::<4>();
            for (sample, &chunk) in samples.iter_mut().zip(chunks) {
                *sample = f32::from_ne_bytes(chunk);
            }

            let frame = analyzer.process(&samples);
            tx.send_if_modified(|current| {
                if *current == frame {
                    false
                } else {
                    *current = frame;
                    true
                }
            });
        }

        drop(stream);

        if !stop_flag.load(Ordering::Acquire) {
            wait_interruptible(&stop_flag, &wake, backoff);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }
}

fn open_monitor_stream() -> Result<Simple, pulse::error::PAErr> {
    let spec = Spec {
        format: Format::FLOAT32NE,
        channels: 1,
        rate: SAMPLE_RATE,
    };
    debug_assert!(spec.is_valid());

    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: (FFT_SIZE * std::mem::size_of::<f32>()) as u32,
    };

    Simple::new(
        None,
        "wavepill",
        Direction::Record,
        Some(MONITOR_DEVICE),
        "spectrum-capture",
        &spec,
        None,
        Some(&attr),
    )
}

fn publish_silent(
    tx: &watch::Sender<SpectrumFrame>,
    analyzer: &mut SpectrumAnalyzer,
    bar_count: usize,
) {
    analyzer.reset();
    let silent = SpectrumFrame::silent(bar_count);
    tx.send_if_modified(|current| {
        if *current == silent {
            false
        } else {
            *current = silent;
            true
        }
    });
}

fn wait_interruptible(stop_flag: &AtomicBool, wake: &(Mutex<()>, Condvar), duration: Duration) {
    let (lock, cvar) = wake;
    let Ok(guard) = lock.lock() else {
        return;
    };
    let _ = cvar.wait_timeout_while(guard, duration, |_| !stop_flag.load(Ordering::Acquire));
}
