//! PipeWire/PulseAudio sink-input introspection fallback for apps without MPRIS.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::introspect::SinkInputInfo;
use pulse::context::subscribe::{Facility, InterestMaskSet};
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::error::PAErr;
use pulse::mainloop::standard::Mainloop;
use pulse::operation::{Operation, State as OperationState};
use pulse::proplist::properties;
use pulse::time::MicroSeconds;
use tokio::sync::watch;

use super::types::{ControlMode, MediaSnapshot, PlaybackStatus};

const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(2);
const POLL_TIMEOUT_US: u64 = 25_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_OP_TIMEOUT: Duration = Duration::from_millis(50);
const STOP_JOIN_TIMEOUT: Duration = Duration::from_millis(150);

/// Normalized metadata for a PipeWire/PulseAudio sink-input stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SinkInputInfoData {
    pub index: u32,
    pub corked: bool,
    pub is_muted: bool,
    pub app_name: String,
    pub raw_app_name: Option<String>,
    pub process_binary: Option<String>,
    pub application_id: Option<String>,
    pub node_name: Option<String>,
    pub media_name: Option<String>,
    pub media_title: Option<String>,
    pub media_artist: Option<String>,
    pub stream_name: Option<String>,
}

impl SinkInputInfoData {
    /// Extracts a normalized [`SinkInputInfoData`] from a `libpulse` [`SinkInputInfo`].
    pub fn from_pulse(info: &SinkInputInfo<'_>) -> Self {
        let props = &info.proplist;
        let raw_app_name = normalize_opt(props.get_str(properties::APPLICATION_NAME));
        let process_binary = normalize_opt(props.get_str(properties::APPLICATION_PROCESS_BINARY));
        let application_id = normalize_opt(props.get_str(properties::APPLICATION_ID));
        let node_name = normalize_opt(props.get_str("node.name"));
        let media_name = normalize_opt(props.get_str(properties::MEDIA_NAME));
        let media_title = normalize_opt(props.get_str(properties::MEDIA_TITLE));
        let media_artist = normalize_opt(props.get_str(properties::MEDIA_ARTIST));
        let stream_name = normalize_str(info.name.as_deref());

        let app_name = resolve_app_name(
            raw_app_name.as_deref(),
            node_name.as_deref(),
            process_binary.as_deref(),
            application_id.as_deref(),
        );

        Self {
            index: info.index,
            corked: info.corked,
            is_muted: info.mute,
            app_name,
            raw_app_name,
            process_binary,
            application_id,
            node_name,
            media_name,
            media_title,
            media_artist,
            stream_name,
        }
    }
}

/// Pure parser building a [`SinkInputInfoData`] from raw property key-value pairs.
pub fn parse_sink_input_from_pairs<'a>(
    index: u32,
    corked: bool,
    is_muted: bool,
    stream_name: Option<&'a str>,
    props: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> SinkInputInfoData {
    let mut raw_app_name = None;
    let mut process_binary = None;
    let mut application_id = None;
    let mut node_name = None;
    let mut media_name = None;
    let mut media_title = None;
    let mut media_artist = None;

    for (key, value) in props {
        let clean = normalize_str(Some(value));
        match key {
            "application.name" => raw_app_name = clean,
            "application.process.binary" => process_binary = clean,
            "application.id" => application_id = clean,
            "node.name" => node_name = clean,
            "media.name" => media_name = clean,
            "media.title" => media_title = clean,
            "media.artist" => media_artist = clean,
            _ => {}
        }
    }

    let app_name = resolve_app_name(
        raw_app_name.as_deref(),
        node_name.as_deref(),
        process_binary.as_deref(),
        application_id.as_deref(),
    );

    SinkInputInfoData {
        index,
        corked,
        is_muted,
        app_name,
        raw_app_name,
        process_binary,
        application_id,
        node_name,
        media_name,
        media_title,
        media_artist,
        stream_name: normalize_str(stream_name),
    }
}

/// Returns `true` when `input` matches any configured entry in `ignore_apps` (case-insensitive).
pub fn is_sink_input_ignored(input: &SinkInputInfoData, ignore_apps: &[String]) -> bool {
    let candidates = [
        Some(input.app_name.as_str()),
        input.raw_app_name.as_deref(),
        input.process_binary.as_deref(),
        input.application_id.as_deref(),
        input.node_name.as_deref(),
    ];

    ignore_apps.iter().any(|ignored| {
        let needle = ignored.trim();
        !needle.is_empty()
            && candidates
                .iter()
                .flatten()
                .any(|candidate| candidate.eq_ignore_ascii_case(needle))
    })
}

/// Deterministically selects an active (non-corked, non-ignored) sink-input stream.
///
/// Prefers unmuted streams over muted ones, preserves `preferred_index` within the same
/// mute tier, and breaks remaining ties by highest stream index (most recently opened).
pub fn select_active_sink_input<'a>(
    inputs: &'a [SinkInputInfoData],
    ignore_apps: &[String],
    preferred_index: Option<u32>,
) -> Option<&'a SinkInputInfoData> {
    inputs
        .iter()
        .filter(|input| !input.corked && !is_sink_input_ignored(input, ignore_apps))
        .max_by_key(|input| {
            (
                !input.is_muted,
                preferred_index == Some(input.index),
                input.index,
            )
        })
}

/// Converts a [`SinkInputInfoData`] into a [`MediaSnapshot`] with [`ControlMode::PipeWireMute`].
pub fn sink_input_to_snapshot(input: &SinkInputInfoData) -> MediaSnapshot {
    let title = input
        .media_title
        .as_deref()
        .or(input.media_name.as_deref())
        .or(input.stream_name.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or(&input.app_name)
        .to_owned();

    let status = if !input.corked && !input.is_muted {
        PlaybackStatus::Playing
    } else {
        PlaybackStatus::Paused
    };

    MediaSnapshot {
        title,
        artist: input.media_artist.clone().unwrap_or_default(),
        app_name: input.app_name.clone(),
        art_url: None,
        status,
        control_mode: ControlMode::PipeWireMute,
        can_go_next: false,
        can_go_previous: false,
        is_muted: input.is_muted,
    }
}

fn normalize_opt(value: Option<String>) -> Option<String> {
    value.and_then(|s| normalize_str(Some(&s)))
}

fn normalize_str(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn resolve_app_name(
    raw_app_name: Option<&str>,
    node_name: Option<&str>,
    process_binary: Option<&str>,
    application_id: Option<&str>,
) -> String {
    raw_app_name
        .or(node_name)
        .or(process_binary)
        .or(application_id)
        .unwrap_or("Audio")
        .to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipeWireCommand {
    ToggleMute(u32),
}

/// Background PipeWire/PulseAudio sink-input monitor running on a dedicated OS thread.
pub(crate) struct PipeWireMonitor {
    stop_flag: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    cmd_tx: mpsc::Sender<PipeWireCommand>,
    inputs_rx: watch::Receiver<Vec<SinkInputInfoData>>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl PipeWireMonitor {
    /// Spawns the PipeWire introspection thread and returns a handle to subscribe and send mute commands.
    pub(crate) fn start() -> Self {
        let (inputs_tx, inputs_rx) = watch::channel(Vec::new());
        let (cmd_tx, cmd_rx) = mpsc::channel::<PipeWireCommand>();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));

        let thread_stop = Arc::clone(&stop_flag);
        let thread_wake = Arc::clone(&wake);

        let handle = thread::Builder::new()
            .name("wavepill-pipewire".to_owned())
            .spawn(move || {
                monitor_loop(inputs_tx, cmd_rx, thread_stop, thread_wake);
            })
            .expect("failed to spawn wavepill-pipewire thread");

        Self {
            stop_flag,
            wake,
            cmd_tx,
            inputs_rx,
            handle: Mutex::new(Some(handle)),
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Vec<SinkInputInfoData>> {
        self.inputs_rx.clone()
    }

    pub(crate) fn command_sender(&self) -> mpsc::Sender<PipeWireCommand> {
        self.cmd_tx.clone()
    }

    pub(crate) fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        let (_lock, cvar) = &*self.wake;
        cvar.notify_all();

        let Ok(mut guard) = self.handle.lock() else {
            return;
        };
        let Some(handle) = guard.take() else {
            return;
        };

        let deadline = Instant::now() + STOP_JOIN_TIMEOUT;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }

        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

impl Drop for PipeWireMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn monitor_loop(
    inputs_tx: watch::Sender<Vec<SinkInputInfoData>>,
    cmd_rx: mpsc::Receiver<PipeWireCommand>,
    stop_flag: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
) {
    let mut backoff = INITIAL_BACKOFF;
    let mut warned_disconnect = false;

    while !stop_flag.load(Ordering::Acquire) {
        match run_pulse_session(&inputs_tx, &cmd_rx, &stop_flag, &mut warned_disconnect) {
            SessionOutcome::Stopped => break,
            SessionOutcome::Disconnected => {
                publish_inputs(&inputs_tx, Vec::new());
                // Drain stale commands accumulated while disconnected.
                while cmd_rx.try_recv().is_ok() {}
                wait_interruptible(&stop_flag, &wake, backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

enum SessionOutcome {
    Stopped,
    Disconnected,
}

type SinkInputListOp = Operation<dyn FnMut(ListResult<&SinkInputInfo<'_>>)>;

fn run_pulse_session(
    inputs_tx: &watch::Sender<Vec<SinkInputInfoData>>,
    cmd_rx: &mpsc::Receiver<PipeWireCommand>,
    stop_flag: &AtomicBool,
    warned_disconnect: &mut bool,
) -> SessionOutcome {
    let Some(mut mainloop) = Mainloop::new() else {
        if !*warned_disconnect {
            tracing::warn!("Failed to allocate PulseAudio mainloop; PipeWire fallback inactive");
            *warned_disconnect = true;
        }
        return SessionOutcome::Disconnected;
    };

    let mut proplist = match pulse::proplist::Proplist::new() {
        Some(pl) => pl,
        None => return SessionOutcome::Disconnected,
    };
    let _ = proplist.set_str(properties::APPLICATION_NAME, "wavepill");

    let mut context = match Context::new_with_proplist(&mainloop, "wavepill-media", &proplist) {
        Some(ctx) => ctx,
        None => return SessionOutcome::Disconnected,
    };

    if let Err(err) = context.connect(None, ContextFlagSet::NOAUTOSPAWN, None) {
        if !*warned_disconnect {
            tracing::warn!(error = %err, "Failed to connect PulseAudio introspection context");
            *warned_disconnect = true;
        }
        return SessionOutcome::Disconnected;
    }

    let connect_deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        if stop_flag.load(Ordering::Acquire) {
            context.disconnect();
            return SessionOutcome::Stopped;
        }

        match context.get_state() {
            ContextState::Ready => break,
            ContextState::Failed | ContextState::Terminated => {
                if !*warned_disconnect {
                    tracing::warn!(
                        "PulseAudio/PipeWire introspection unavailable; retrying in background"
                    );
                    *warned_disconnect = true;
                }
                context.disconnect();
                return SessionOutcome::Disconnected;
            }
            _ => {
                if Instant::now() >= connect_deadline {
                    if !*warned_disconnect {
                        tracing::warn!("Timed out connecting PulseAudio introspection context");
                        *warned_disconnect = true;
                    }
                    context.disconnect();
                    return SessionOutcome::Disconnected;
                }
                if pump_mainloop(&mut mainloop, POLL_TIMEOUT_US).is_err() {
                    context.disconnect();
                    return SessionOutcome::Disconnected;
                }
            }
        }
    }

    if *warned_disconnect {
        tracing::info!("Reconnected PipeWire/PulseAudio introspection context");
    } else {
        tracing::debug!("Connected PipeWire/PulseAudio introspection context");
    }
    *warned_disconnect = false;

    let needs_refresh = Rc::new(Cell::new(true));
    let sub_refresh = Rc::clone(&needs_refresh);
    context.set_subscribe_callback(Some(Box::new(move |facility, _op, _idx| {
        if matches!(facility, Some(Facility::SinkInput)) {
            sub_refresh.set(true);
        }
    })));

    let sub_op = context.subscribe(InterestMaskSet::SINK_INPUT, |_success| {});
    drain_operation(&mut mainloop, &context, &sub_op, stop_flag);
    drop(sub_op);

    let pending_items: Rc<RefCell<Vec<SinkInputInfoData>>> = Rc::new(RefCell::new(Vec::new()));
    let mut latest_inputs: Vec<SinkInputInfoData> = Vec::new();
    let mut list_op: Option<SinkInputListOp> = None;

    while !stop_flag.load(Ordering::Acquire) {
        if context.get_state() != ContextState::Ready {
            break;
        }

        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                PipeWireCommand::ToggleMute(target_index) => {
                    if let Some(entry) = latest_inputs
                        .iter_mut()
                        .find(|item| item.index == target_index)
                    {
                        let next_mute = !entry.is_muted;
                        let _ =
                            context
                                .introspect()
                                .set_sink_input_mute(target_index, next_mute, None);
                        entry.is_muted = next_mute;
                        publish_inputs(inputs_tx, latest_inputs.clone());
                        needs_refresh.set(true);
                    }
                }
            }
        }

        if let Some(op) = list_op.as_ref()
            && op.get_state() != OperationState::Running
        {
            let finished_state = op.get_state();
            list_op = None;
            if finished_state == OperationState::Done {
                latest_inputs = pending_items.borrow_mut().drain(..).collect();
                publish_inputs(inputs_tx, latest_inputs.clone());
            } else {
                pending_items.borrow_mut().clear();
            }
        }

        if list_op.is_none() && needs_refresh.get() && context.get_state() == ContextState::Ready {
            needs_refresh.set(false);
            pending_items.borrow_mut().clear();
            let collector = Rc::clone(&pending_items);
            let op = context.introspect().get_sink_input_info_list(move |res| {
                if let ListResult::Item(info) = res {
                    collector
                        .borrow_mut()
                        .push(SinkInputInfoData::from_pulse(info));
                }
            });
            list_op = Some(op);
        }

        if pump_mainloop(&mut mainloop, POLL_TIMEOUT_US).is_err() {
            break;
        }
    }

    if let Some(op) = list_op {
        drain_operation(&mut mainloop, &context, &op, stop_flag);
    }

    context.set_subscribe_callback(None);
    context.disconnect();

    if stop_flag.load(Ordering::Acquire) {
        SessionOutcome::Stopped
    } else {
        SessionOutcome::Disconnected
    }
}

fn drain_operation<T: ?Sized>(
    mainloop: &mut Mainloop,
    context: &Context,
    op: &Operation<T>,
    stop_flag: &AtomicBool,
) {
    let deadline = Instant::now() + DRAIN_OP_TIMEOUT;
    while op.get_state() == OperationState::Running
        && context.get_state() == ContextState::Ready
        && !stop_flag.load(Ordering::Acquire)
        && Instant::now() < deadline
    {
        if pump_mainloop(mainloop, 5_000).is_err() {
            break;
        }
    }
}

fn pump_mainloop(mainloop: &mut Mainloop, timeout_us: u64) -> Result<(), PAErr> {
    mainloop.prepare(Some(MicroSeconds(timeout_us)))?;
    mainloop.poll()?;
    mainloop.dispatch()?;
    Ok(())
}

fn publish_inputs(tx: &watch::Sender<Vec<SinkInputInfoData>>, next: Vec<SinkInputInfoData>) {
    tx.send_if_modified(|current| {
        if *current != next {
            *current = next;
            true
        } else {
            false
        }
    });
}

fn wait_interruptible(stop_flag: &AtomicBool, wake: &(Mutex<()>, Condvar), duration: Duration) {
    let (lock, cvar) = wake;
    let Ok(guard) = lock.lock() else {
        thread::sleep(duration.min(Duration::from_millis(50)));
        return;
    };
    if !stop_flag.load(Ordering::Acquire) {
        let _ = cvar.wait_timeout(guard, duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input(
        index: u32,
        corked: bool,
        is_muted: bool,
        app_name: &str,
        media_name: Option<&str>,
    ) -> SinkInputInfoData {
        let mut props = vec![("application.name", app_name)];
        if let Some(name) = media_name {
            props.push(("media.name", name));
        }
        parse_sink_input_from_pairs(index, corked, is_muted, None, props)
    }

    #[test]
    fn parse_sink_input_extracts_properties_and_fallbacks() {
        let zen = parse_sink_input_from_pairs(
            232,
            false,
            false,
            Some("Playback"),
            [
                ("application.name", "  Zen "),
                ("application.process.binary", "zen-bin"),
                ("application.id", "app.zen_browser.zen"),
                ("node.name", "Zen"),
                ("media.name", "Lo-Fi Radio - YouTube"),
            ],
        );

        assert_eq!(zen.index, 232);
        assert!(!zen.corked);
        assert!(!zen.is_muted);
        assert_eq!(zen.app_name, "Zen");
        assert_eq!(zen.process_binary.as_deref(), Some("zen-bin"));
        assert_eq!(zen.media_name.as_deref(), Some("Lo-Fi Radio - YouTube"));

        let fallback = parse_sink_input_from_pairs(
            99,
            false,
            true,
            None,
            [("application.process.binary", "mpv")],
        );
        assert_eq!(fallback.app_name, "mpv");
        assert!(fallback.is_muted);
    }

    #[test]
    fn select_ignores_corked_and_ignored_apps_case_insensitively() {
        let inputs = vec![
            sample_input(10, true, false, "Discord", Some("Voice")),
            sample_input(11, false, false, "Zen", Some("Ignored Video")),
            sample_input(12, false, false, "mpv", Some("movie.mkv")),
        ];
        let ignore = vec!["zen".to_owned(), "  ".to_owned()];

        let selected = select_active_sink_input(&inputs, &ignore, None)
            .expect("should select uncorked non-ignored mpv stream");
        assert_eq!(selected.index, 12);
        assert_eq!(selected.app_name, "mpv");
    }

    #[test]
    fn select_prefers_unmuted_then_preferred_index_then_highest_index() {
        let inputs = vec![
            sample_input(5, false, true, "MutedApp", Some("Muted")),
            sample_input(6, false, false, "Game", Some("BGM")),
            sample_input(9, false, false, "Browser", Some("Stream")),
        ];

        // Prefers preferred_index among unmuted streams.
        let kept = select_active_sink_input(&inputs, &[], Some(6)).unwrap();
        assert_eq!(kept.index, 6);

        // Without preference, picks highest-index unmuted stream.
        let latest = select_active_sink_input(&inputs, &[], None).unwrap();
        assert_eq!(latest.index, 9);

        // If only muted uncorked streams exist, still returns one so user can unmute.
        let only_muted = vec![sample_input(5, false, true, "MutedApp", Some("Muted"))];
        let muted = select_active_sink_input(&only_muted, &[], None).unwrap();
        assert_eq!(muted.index, 5);
        assert!(muted.is_muted);
    }

    #[test]
    fn sink_input_to_snapshot_maps_status_title_and_mute_state() {
        let playing = sample_input(20, false, false, "Zen", Some("YouTube Video"));
        let snap_playing = sink_input_to_snapshot(&playing);
        assert_eq!(snap_playing.control_mode, ControlMode::PipeWireMute);
        assert_eq!(snap_playing.status, PlaybackStatus::Playing);
        assert_eq!(snap_playing.app_name, "Zen");
        assert_eq!(snap_playing.title, "YouTube Video");
        assert!(!snap_playing.is_muted);
        assert!(!snap_playing.can_go_next);
        assert!(!snap_playing.can_go_previous);

        let muted_no_media_name = sample_input(21, false, true, "Discord", None);
        let snap_muted = sink_input_to_snapshot(&muted_no_media_name);
        assert_eq!(snap_muted.control_mode, ControlMode::PipeWireMute);
        assert_eq!(snap_muted.status, PlaybackStatus::Paused);
        assert_eq!(snap_muted.app_name, "Discord");
        assert_eq!(snap_muted.title, "Discord");
        assert!(snap_muted.is_muted);
    }

    #[test]
    fn ignore_matches_binary_and_application_id() {
        let input = parse_sink_input_from_pairs(
            77,
            false,
            false,
            None,
            [
                ("application.name", "Mozilla zen"),
                ("application.process.binary", "zen-bin"),
                ("application.id", "app.zen_browser.zen"),
            ],
        );

        assert!(is_sink_input_ignored(&input, &["ZEN-BIN".to_owned()]));
        assert!(is_sink_input_ignored(
            &input,
            &["app.zen_browser.zen".to_owned()]
        ));
        assert!(!is_sink_input_ignored(&input, &["spotify".to_owned()]));
    }
}
