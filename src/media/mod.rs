//! Unified media controller orchestrating MPRIS2 over D-Bus with PipeWire fallback.

pub mod mpris;
pub mod pipewire;
pub mod types;

pub use types::{ControlMode, MediaCommand, MediaSnapshot, PlaybackStatus};

use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};

use self::mpris::{MprisDispatch, MprisPlayer, player_to_snapshot, run_mpris_monitor};
use self::pipewire::{
    PipeWireCommand, PipeWireMonitor, SinkInputInfoData, select_active_sink_input,
    sink_input_to_snapshot,
};

const RUNTIME_WORKER_THREADS: usize = 2;
const STOP_JOIN_TIMEOUT: Duration = Duration::from_millis(250);
const TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(80);
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(50);

/// Active backend target currently driving [`MediaSnapshot`] and command routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActiveTarget {
    None,
    Mpris(String),
    PipeWire(u32),
}

/// Pure reconciliation between the prioritized MPRIS player and active PipeWire sink-inputs.
pub(crate) fn reconcile_media_state(
    mpris_player: Option<&MprisPlayer>,
    sink_inputs: &[SinkInputInfoData],
    ignore_apps: &[String],
    last_target: &ActiveTarget,
) -> (MediaSnapshot, ActiveTarget) {
    if let Some(player) = mpris_player
        && player.status == PlaybackStatus::Playing
    {
        return (
            player_to_snapshot(player),
            ActiveTarget::Mpris(player.bus_name.clone()),
        );
    }

    let preferred_pw = match last_target {
        ActiveTarget::PipeWire(index) => Some(*index),
        _ => None,
    };

    if let Some(pw) = select_active_sink_input(sink_inputs, ignore_apps, preferred_pw) {
        let was_controlling_this_pw =
            matches!(last_target, ActiveTarget::PipeWire(idx) if *idx == pw.index);
        let has_paused_mpris =
            matches!(mpris_player, Some(player) if player.status == PlaybackStatus::Paused);

        if !pw.is_muted || was_controlling_this_pw || !has_paused_mpris {
            return (sink_input_to_snapshot(pw), ActiveTarget::PipeWire(pw.index));
        }
    }

    if let Some(player) = mpris_player {
        return (
            player_to_snapshot(player),
            ActiveTarget::Mpris(player.bus_name.clone()),
        );
    }

    (MediaSnapshot::default(), ActiveTarget::None)
}

/// Background media controller combining MPRIS2 D-Bus signals and PipeWire sink-input fallback.
pub struct MediaController {
    snapshot_rx: watch::Receiver<MediaSnapshot>,
    cmd_tx: mpsc::UnboundedSender<MediaCommand>,
    shutdown_tx: watch::Sender<bool>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl MediaController {
    /// Spawns a dedicated controller thread with a lightweight Tokio runtime,
    /// publishes [`MediaSnapshot`] updates over a `watch` channel, and routes [`MediaCommand`]s.
    pub fn start(ignore_apps: Vec<String>) -> Self {
        let (snapshot_tx, snapshot_rx) = watch::channel(MediaSnapshot::default());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<MediaCommand>();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = thread::Builder::new()
            .name("wavepill-media".to_owned())
            .spawn(move || {
                run_controller_thread(ignore_apps, snapshot_tx, cmd_rx, shutdown_rx);
            })
            .expect("failed to spawn wavepill-media thread");

        Self {
            snapshot_rx,
            cmd_tx,
            shutdown_tx,
            handle: Mutex::new(Some(handle)),
        }
    }

    /// Returns a receiver that yields the latest [`MediaSnapshot`] changes.
    pub fn subscribe(&self) -> watch::Receiver<MediaSnapshot> {
        self.snapshot_rx.clone()
    }

    /// Returns the most recent [`MediaSnapshot`].
    pub fn latest(&self) -> MediaSnapshot {
        self.snapshot_rx.borrow().clone()
    }

    /// Enqueues a [`MediaCommand`] for asynchronous execution against the active target.
    pub fn send_command(&self, cmd: MediaCommand) {
        if self.cmd_tx.send(cmd).is_err() {
            tracing::debug!(?cmd, "Dropped media command because controller is stopped");
        }
    }

    /// Signals a clean shutdown and waits briefly for the background controller thread to exit.
    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);

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

impl Drop for MediaController {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_controller_thread(
    ignore_apps: Vec<String>,
    snapshot_tx: watch::Sender<MediaSnapshot>,
    cmd_rx: mpsc::UnboundedReceiver<MediaCommand>,
    shutdown_rx: watch::Receiver<bool>,
) {
    if *shutdown_rx.borrow() {
        return;
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(RUNTIME_WORKER_THREADS)
        .thread_name("wavepill-media-rt")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(error = %err, "Failed to build Tokio runtime for media controller");
            return;
        }
    };

    let pipewire = PipeWireMonitor::start();
    let pw_rx = pipewire.subscribe();
    let pw_cmd_tx = pipewire.command_sender();

    rt.block_on(run_coordinator(
        ignore_apps,
        snapshot_tx,
        cmd_rx,
        shutdown_rx,
        pw_cmd_tx,
        pw_rx,
    ));

    pipewire.stop();
    rt.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
}

async fn run_coordinator(
    ignore_apps: Vec<String>,
    snapshot_tx: watch::Sender<MediaSnapshot>,
    mut cmd_rx: mpsc::UnboundedReceiver<MediaCommand>,
    mut shutdown_rx: watch::Receiver<bool>,
    pw_cmd_tx: std::sync::mpsc::Sender<PipeWireCommand>,
    mut pw_rx: watch::Receiver<Vec<SinkInputInfoData>>,
) {
    let (mpris_active_tx, mut mpris_active_rx) = watch::channel(None);
    let (mpris_cmd_tx, mpris_cmd_rx) = mpsc::unbounded_channel::<MprisDispatch>();

    let mpris_handle = tokio::spawn(run_mpris_monitor(
        ignore_apps.clone(),
        mpris_active_tx,
        mpris_cmd_rx,
        shutdown_rx.clone(),
    ));

    let mut active_target = ActiveTarget::None;
    reconcile_and_publish(
        &mpris_active_rx,
        &pw_rx,
        &ignore_apps,
        &mut active_target,
        &snapshot_tx,
    );

    let mut mpris_alive = true;
    let mut pw_alive = true;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            res = mpris_active_rx.changed(), if mpris_alive => {
                if res.is_ok() {
                    reconcile_and_publish(
                        &mpris_active_rx,
                        &pw_rx,
                        &ignore_apps,
                        &mut active_target,
                        &snapshot_tx,
                    );
                } else {
                    mpris_alive = false;
                    tracing::warn!("MPRIS monitor channel closed; disabling MPRIS watch branch");
                }
            }
            res = pw_rx.changed(), if pw_alive => {
                if res.is_ok() {
                    reconcile_and_publish(
                        &mpris_active_rx,
                        &pw_rx,
                        &ignore_apps,
                        &mut active_target,
                        &snapshot_tx,
                    );
                } else {
                    pw_alive = false;
                    tracing::warn!("PipeWire monitor channel closed; disabling PipeWire watch branch");
                }
            }
            maybe_cmd = cmd_rx.recv() => {
                let Some(cmd) = maybe_cmd else {
                    break;
                };
                dispatch_command(cmd, &active_target, &mpris_cmd_tx, &pw_cmd_tx);
            }
        }
    }

    drop(mpris_cmd_tx);
    let _ = tokio::time::timeout(TASK_SHUTDOWN_TIMEOUT, mpris_handle).await;
}

fn reconcile_and_publish(
    mpris_active_rx: &watch::Receiver<Option<MprisPlayer>>,
    pw_rx: &watch::Receiver<Vec<SinkInputInfoData>>,
    ignore_apps: &[String],
    active_target: &mut ActiveTarget,
    snapshot_tx: &watch::Sender<MediaSnapshot>,
) {
    let mpris_player = mpris_active_rx.borrow().clone();
    let sink_inputs = pw_rx.borrow().clone();

    let (snapshot, next_target) = reconcile_media_state(
        mpris_player.as_ref(),
        &sink_inputs,
        ignore_apps,
        active_target,
    );
    *active_target = next_target;

    snapshot_tx.send_if_modified(|current| {
        if *current != snapshot {
            *current = snapshot;
            true
        } else {
            false
        }
    });
}

fn dispatch_command(
    cmd: MediaCommand,
    active_target: &ActiveTarget,
    mpris_cmd_tx: &mpsc::UnboundedSender<MprisDispatch>,
    pw_cmd_tx: &std::sync::mpsc::Sender<PipeWireCommand>,
) {
    match (cmd, active_target) {
        (
            MediaCommand::PlayPause | MediaCommand::Next | MediaCommand::Previous,
            ActiveTarget::Mpris(bus_name),
        ) => {
            let _ = mpris_cmd_tx.send(MprisDispatch {
                bus_name: bus_name.clone(),
                command: cmd,
            });
        }
        (MediaCommand::ToggleMute | MediaCommand::PlayPause, ActiveTarget::PipeWire(index)) => {
            let _ = pw_cmd_tx.send(PipeWireCommand::ToggleMute(*index));
        }
        _ => {
            tracing::debug!(
                ?cmd,
                ?active_target,
                "Ignoring media command unsupported by active target"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::mpris::{MPRIS_BUS_PREFIX, MprisMetadata};
    use crate::media::pipewire::parse_sink_input_from_pairs;

    fn sample_mpris(suffix: &str, identity: &str, status: PlaybackStatus) -> MprisPlayer {
        MprisPlayer {
            bus_name: format!("{MPRIS_BUS_PREFIX}{suffix}"),
            identity: identity.to_owned(),
            desktop_entry: Some(suffix.to_owned()),
            status,
            metadata: MprisMetadata {
                title: "Song".to_owned(),
                artist: "Artist".to_owned(),
                art_url: None,
            },
            can_go_next: true,
            can_go_previous: true,
        }
    }

    fn sample_pw(index: u32, is_muted: bool, app: &str, media: &str) -> SinkInputInfoData {
        parse_sink_input_from_pairs(
            index,
            false,
            is_muted,
            None,
            [("application.name", app), ("media.name", media)],
        )
    }

    #[test]
    fn reconcile_prefers_playing_mpris_over_pipewire_fallback() {
        let mpris = sample_mpris("spotify", "Spotify", PlaybackStatus::Playing);
        let pw = vec![sample_pw(42, false, "Zen", "YouTube")];

        let (snapshot, target) = reconcile_media_state(Some(&mpris), &pw, &[], &ActiveTarget::None);

        assert_eq!(snapshot.control_mode, ControlMode::Mpris);
        assert_eq!(snapshot.status, PlaybackStatus::Playing);
        assert_eq!(snapshot.app_name, "Spotify");
        assert_eq!(
            target,
            ActiveTarget::Mpris("org.mpris.MediaPlayer2.spotify".to_owned())
        );
    }

    #[test]
    fn reconcile_falls_back_to_unmuted_pipewire_when_mpris_is_paused() {
        let mpris = sample_mpris("spotify", "Spotify", PlaybackStatus::Paused);
        let pw = vec![sample_pw(7, false, "Discord", "Voice Call")];

        let (snapshot, target) = reconcile_media_state(Some(&mpris), &pw, &[], &ActiveTarget::None);

        assert_eq!(snapshot.control_mode, ControlMode::PipeWireMute);
        assert_eq!(snapshot.status, PlaybackStatus::Playing);
        assert_eq!(snapshot.app_name, "Discord");
        assert_eq!(snapshot.title, "Voice Call");
        assert!(!snapshot.is_muted);
        assert_eq!(target, ActiveTarget::PipeWire(7));
    }

    #[test]
    fn reconcile_keeps_muted_pipewire_target_if_user_was_controlling_it() {
        let mpris = sample_mpris("spotify", "Spotify", PlaybackStatus::Paused);
        let pw = vec![sample_pw(7, true, "Discord", "Voice Call")];

        let (snapshot, target) =
            reconcile_media_state(Some(&mpris), &pw, &[], &ActiveTarget::PipeWire(7));

        assert_eq!(snapshot.control_mode, ControlMode::PipeWireMute);
        assert_eq!(snapshot.status, PlaybackStatus::Paused);
        assert!(snapshot.is_muted);
        assert_eq!(target, ActiveTarget::PipeWire(7));
    }

    #[test]
    fn reconcile_uses_paused_mpris_over_unrelated_muted_pipewire() {
        let mpris = sample_mpris("spotify", "Spotify", PlaybackStatus::Paused);
        let pw = vec![sample_pw(7, true, "Discord", "Voice Call")];

        let (snapshot, target) = reconcile_media_state(Some(&mpris), &pw, &[], &ActiveTarget::None);

        assert_eq!(snapshot.control_mode, ControlMode::Mpris);
        assert_eq!(snapshot.status, PlaybackStatus::Paused);
        assert_eq!(snapshot.app_name, "Spotify");
        assert_eq!(
            target,
            ActiveTarget::Mpris("org.mpris.MediaPlayer2.spotify".to_owned())
        );
    }

    #[test]
    fn reconcile_returns_default_when_no_mpris_and_only_ignored_pipewire() {
        let pw = vec![sample_pw(12, false, "Zen", "Muted Tab")];
        let ignore = vec!["zen".to_owned()];

        let (snapshot, target) = reconcile_media_state(None, &pw, &ignore, &ActiveTarget::None);

        assert_eq!(snapshot, MediaSnapshot::default());
        assert_eq!(target, ActiveTarget::None);
    }

    #[test]
    fn dispatch_routes_mpris_and_pipewire_commands_cleanly() {
        let (mpris_tx, mut mpris_rx) = mpsc::unbounded_channel();
        let (pw_tx, pw_rx) = std::sync::mpsc::channel();

        let mpris_target = ActiveTarget::Mpris("org.mpris.MediaPlayer2.spotify".to_owned());
        dispatch_command(MediaCommand::PlayPause, &mpris_target, &mpris_tx, &pw_tx);
        dispatch_command(MediaCommand::Next, &mpris_target, &mpris_tx, &pw_tx);
        dispatch_command(MediaCommand::Previous, &mpris_target, &mpris_tx, &pw_tx);
        dispatch_command(MediaCommand::ToggleMute, &mpris_target, &mpris_tx, &pw_tx);

        assert_eq!(
            mpris_rx.try_recv().unwrap(),
            MprisDispatch {
                bus_name: "org.mpris.MediaPlayer2.spotify".to_owned(),
                command: MediaCommand::PlayPause,
            }
        );
        assert_eq!(mpris_rx.try_recv().unwrap().command, MediaCommand::Next);
        assert_eq!(mpris_rx.try_recv().unwrap().command, MediaCommand::Previous);
        assert!(mpris_rx.try_recv().is_err());
        assert!(pw_rx.try_recv().is_err());

        let pw_target = ActiveTarget::PipeWire(232);
        dispatch_command(MediaCommand::ToggleMute, &pw_target, &mpris_tx, &pw_tx);
        dispatch_command(MediaCommand::PlayPause, &pw_target, &mpris_tx, &pw_tx);
        dispatch_command(MediaCommand::Next, &pw_target, &mpris_tx, &pw_tx);

        assert_eq!(pw_rx.try_recv().unwrap(), PipeWireCommand::ToggleMute(232));
        assert_eq!(pw_rx.try_recv().unwrap(), PipeWireCommand::ToggleMute(232));
        assert!(pw_rx.try_recv().is_err());
        assert!(mpris_rx.try_recv().is_err());

        dispatch_command(
            MediaCommand::PlayPause,
            &ActiveTarget::None,
            &mpris_tx,
            &pw_tx,
        );
        assert!(mpris_rx.try_recv().is_err());
        assert!(pw_rx.try_recv().is_err());
    }

    #[test]
    fn controller_start_subscribe_send_and_stop_are_non_blocking() {
        let controller = MediaController::start(vec!["zen".to_owned(), "spotify".to_owned()]);
        let _rx = controller.subscribe();
        let _latest = controller.latest();

        let start = Instant::now();
        controller.stop();
        // Sending after stop must safely drop without panicking or blocking.
        controller.send_command(MediaCommand::PlayPause);
        controller.send_command(MediaCommand::ToggleMute);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "stop() took unexpectedly long: {:?}",
            start.elapsed()
        );
    }
}
