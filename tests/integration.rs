use std::f32::consts::PI;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gtk4::RevealerTransitionType;
use gtk4_layer_shell::Edge;
use wavepill::audio::SpectrumAnalyzer;
use wavepill::audio::fft::{FFT_SIZE, SAMPLE_RATE};
use wavepill::config::{Config, Corner, MAX_BAR_COUNT, MIN_BAR_COUNT};
use wavepill::ipc::{
    DaemonStatus, IpcCommand, IpcEvent, IpcServer, format_waybar_json, query_status_at,
    send_command_to_path,
};
use wavepill::media::mpris::{
    MPRIS_BUS_PREFIX, MprisMetadata, MprisPlayer, is_mpris_app_ignored, player_to_snapshot,
    prioritize_players,
};
use wavepill::media::pipewire::{
    parse_sink_input_from_pairs, select_active_sink_input, sink_input_to_snapshot,
};
use wavepill::media::{ControlMode, MediaCommand, MediaSnapshot, PlaybackStatus};
use wavepill::state::{IslandEvent, IslandStateMachine, VisibilityState};
use wavepill::ui::equalizer::{EqualizerState, RgbColor};
use wavepill::ui::island::{expanded_layout, format_media_labels, resolve_primary_control};
use wavepill::ui::window::{apply_drag_delta, layer_placement};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempConfigFile {
    path: PathBuf,
}

impl TempConfigFile {
    fn new(name: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("wavepill_it_{pid}_{id}_{name}"));
        Self { path }
    }
}

impl Drop for TempConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("wavepill_it_dir_{pid}_{id}_{name}"));
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn sine_wave(freq_hz: f32, amplitude: f32, len: usize) -> Vec<f32> {
    (0..len)
        .map(|n| amplitude * (2.0 * PI * freq_hz * n as f32 / SAMPLE_RATE as f32).sin())
        .collect()
}

fn mpris_snapshot(
    status: PlaybackStatus,
    app_name: &str,
    title: &str,
    artist: &str,
) -> MediaSnapshot {
    MediaSnapshot {
        title: title.to_owned(),
        artist: artist.to_owned(),
        app_name: app_name.to_owned(),
        art_url: None,
        status,
        control_mode: ControlMode::Mpris,
        can_go_next: true,
        can_go_previous: true,
        is_muted: false,
    }
}

fn paused_mpris_snapshot(app_name: &str, title: &str, artist: &str) -> MediaSnapshot {
    mpris_snapshot(PlaybackStatus::Paused, app_name, title, artist)
}

fn playing_mpris_snapshot(app_name: &str, title: &str, artist: &str) -> MediaSnapshot {
    mpris_snapshot(PlaybackStatus::Playing, app_name, title, artist)
}

#[test]
fn e2e_silence_to_audio_hover_pause_unhover_and_hide_delay_expiration() {
    let config = Config::parse_toml(
        r##"
        corner = "bottom-left"
        margin_x = 24
        margin_y = 32
        hide_delay_ms = 1200
        bar_count = 16
        accent_color = "#bb9af7"
        "##,
    )
    .expect("valid TOML config should parse");

    let mut analyzer = SpectrumAnalyzer::new(config.bar_count);
    let mut machine = IslandStateMachine::new(config.hide_delay());
    let mut equalizer = EqualizerState::new(config.bar_count, &config.accent_color);

    assert_eq!(equalizer.color(), RgbColor::from_u8(0xbb, 0x9a, 0xf7));

    let t0 = Instant::now();

    // 1. Initial silence keeps the island hidden and equalizer inactive.
    let silent_pcm = vec![0.0_f32; FFT_SIZE];
    let silent_frame = analyzer.process(&silent_pcm);
    assert!(!silent_frame.is_audible);
    assert!(silent_frame.bars.iter().all(|&b| b == 0.0));

    let state = machine.handle(IslandEvent::AudioActivity(silent_frame.is_audible), t0);
    assert_eq!(state, VisibilityState::Hidden);
    assert!(!machine.is_visible());

    equalizer.set_active(machine.is_visible());
    equalizer.update_frame(&silent_frame, false);
    assert!(!equalizer.tick());

    // 2. Audible 440 Hz tone reveals the compact pill and animates equalizer bars upward.
    let t1 = t0 + Duration::from_millis(50);
    let audible_pcm = sine_wave(440.0, 0.7, FFT_SIZE);
    let audible_frame = analyzer.process(&audible_pcm);
    assert!(audible_frame.is_audible);
    assert_eq!(audible_frame.bars.len(), 16);
    assert!(audible_frame.bars.iter().any(|&b| b > 0.5));

    let state = machine.handle(IslandEvent::AudioActivity(audible_frame.is_audible), t1);
    assert_eq!(state, VisibilityState::VisibleCompact);
    assert!(machine.is_visible());
    assert!(!machine.is_expanded());

    equalizer.set_active(machine.is_visible());
    equalizer.update_frame(&audible_frame, false);
    assert!(equalizer.tick());
    assert!(equalizer.displayed_bars().iter().any(|&b| b > 0.2));

    // 3. Pointer hover expands the island while audio is active.
    let t2 = t1 + Duration::from_millis(120);
    let state = machine.handle(IslandEvent::PointerEntered, t2);
    assert_eq!(state, VisibilityState::VisibleExpanded);
    assert!(machine.is_visible());
    assert!(machine.is_expanded());

    // 4. Media pauses and audio drops to silence: hover holds VisibleExpanded while equalizer decays.
    let t3 = t2 + Duration::from_millis(200);
    let paused_snapshot = paused_mpris_snapshot("Spotify", "Midnight City", "M83");
    let state = machine.handle(IslandEvent::MediaUpdated(paused_snapshot.clone()), t3);
    assert_eq!(state, VisibilityState::VisibleExpanded);

    let after_pause_frame = analyzer.process(&silent_pcm);
    assert!(!after_pause_frame.is_audible);

    let state = machine.handle(IslandEvent::AudioActivity(after_pause_frame.is_audible), t3);
    assert_eq!(state, VisibilityState::VisibleExpanded);

    let is_paused = paused_snapshot.status == PlaybackStatus::Paused;
    equalizer.update_frame(&after_pause_frame, is_paused);
    assert_eq!(equalizer.target_bars(), &vec![0.0; 16]);
    for _ in 0..80 {
        equalizer.tick();
    }
    assert_eq!(equalizer.displayed_bars(), &vec![0.0; 16]);
    assert!(!equalizer.tick());

    // Even well past hide_delay while hovered, island must stay expanded.
    let t4 = t3 + Duration::from_secs(5);
    assert_eq!(
        machine.handle(IslandEvent::Tick(t4), t4),
        VisibilityState::VisibleExpanded
    );

    // 5. Pointer leaves while paused and silent -> enters HidePending from t_leave.
    let t_leave = t4 + Duration::from_millis(100);
    let state = machine.handle(IslandEvent::PointerLeft, t_leave);
    assert_eq!(state, VisibilityState::HidePending { since: t_leave });
    assert!(machine.is_visible());
    assert!(!machine.is_expanded());

    // 6. Tick 1 ms before hide_delay expires -> remains HidePending.
    let t_before_expiry = t_leave + config.hide_delay() - Duration::from_millis(1);
    assert_eq!(
        machine.handle(IslandEvent::Tick(t_before_expiry), t_before_expiry),
        VisibilityState::HidePending { since: t_leave }
    );
    assert!(machine.is_visible());

    // 7. Tick at exact hide_delay expiration -> transitions to Hidden and deactivates equalizer.
    let t_expiry = t_leave + config.hide_delay();
    assert_eq!(
        machine.handle(IslandEvent::Tick(t_expiry), t_expiry),
        VisibilityState::Hidden
    );
    assert!(!machine.is_visible());

    equalizer.set_active(machine.is_visible());
    assert!(!equalizer.is_active());
    assert!(!equalizer.tick());
}

#[test]
fn e2e_paused_mpris_falls_back_to_active_pipewire_with_ignore_filtering() {
    let config = Config::parse_toml(
        r#"
        hide_delay_ms = 2000
        ignore_apps = ["zen", "Spotify-Ignored"]
        "#,
    )
    .expect("valid TOML config should parse");

    // Simulate MPRIS discovery with one ignored playing player and one paused player.
    let mpris_candidates = vec![
        MprisPlayer {
            bus_name: format!("{MPRIS_BUS_PREFIX}spotify_ignored"),
            identity: "Spotify-Ignored".to_owned(),
            desktop_entry: Some("spotify-ignored".to_owned()),
            status: PlaybackStatus::Playing,
            metadata: MprisMetadata {
                title: "Ignored Track".to_owned(),
                artist: "Ignored Artist".to_owned(),
                art_url: None,
            },
            can_go_next: true,
            can_go_previous: true,
        },
        MprisPlayer {
            bus_name: format!("{MPRIS_BUS_PREFIX}vlc"),
            identity: "VLC media player".to_owned(),
            desktop_entry: Some("vlc".to_owned()),
            status: PlaybackStatus::Paused,
            metadata: MprisMetadata {
                title: "Paused Video".to_owned(),
                artist: String::new(),
                art_url: None,
            },
            can_go_next: false,
            can_go_previous: false,
        },
    ];

    let visible_mpris: Vec<MprisPlayer> = mpris_candidates
        .into_iter()
        .filter(|p| {
            !is_mpris_app_ignored(
                &p.bus_name,
                Some(&p.identity),
                p.desktop_entry.as_deref(),
                &config.ignore_apps,
            )
        })
        .collect();

    let selected_mpris = prioritize_players(&visible_mpris).expect("VLC should remain selectable");
    assert_eq!(selected_mpris.status, PlaybackStatus::Paused);
    let mpris_snapshot = player_to_snapshot(selected_mpris);
    assert_eq!(mpris_snapshot.control_mode, ControlMode::Mpris);
    assert_eq!(
        resolve_primary_control(&mpris_snapshot).command,
        MediaCommand::PlayPause
    );

    // Simulate PipeWire sink-inputs: one ignored ("zen"), one corked, and one active unmuted ("Discord").
    let sink_inputs = vec![
        parse_sink_input_from_pairs(
            10,
            false,
            false,
            None,
            [
                ("application.name", "Zen"),
                ("application.process.binary", "zen"),
                ("media.name", "YouTube"),
            ],
        ),
        parse_sink_input_from_pairs(
            11,
            true,
            false,
            None,
            [("application.name", "mpv"), ("media.name", "Paused Clip")],
        ),
        parse_sink_input_from_pairs(
            12,
            false,
            false,
            None,
            [
                ("application.name", "Discord"),
                ("media.name", "Voice Call"),
            ],
        ),
    ];

    let pw_selected = select_active_sink_input(&sink_inputs, &config.ignore_apps, None)
        .expect("Discord sink-input should be selected over ignored/corked streams");
    assert_eq!(pw_selected.index, 12);

    // Since MPRIS is paused and an active unmuted PipeWire stream exists, snapshot falls back to PipeWire.
    let pw_snapshot = sink_input_to_snapshot(pw_selected);
    assert_eq!(pw_snapshot.control_mode, ControlMode::PipeWireMute);
    assert_eq!(pw_snapshot.status, PlaybackStatus::Playing);
    assert!(!pw_snapshot.is_muted);

    let primary = resolve_primary_control(&pw_snapshot);
    assert_eq!(primary.command, MediaCommand::ToggleMute);
    assert_eq!(primary.tooltip, "Mute");
    assert!(primary.sensitive);
    assert_eq!(
        format_media_labels(&pw_snapshot),
        ("Voice Call".to_owned(), "Discord".to_owned())
    );

    // Active PipeWire playback keeps the island visible even when audio RMS is silent.
    let mut machine = IslandStateMachine::new(config.hide_delay());
    let t0 = Instant::now();
    assert_eq!(
        machine.handle(IslandEvent::MediaUpdated(pw_snapshot), t0),
        VisibilityState::VisibleCompact
    );
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(false), t0),
        VisibilityState::VisibleCompact
    );

    // Muting the PipeWire stream updates control tooltip to Unmute and starts HidePending.
    let muted_pw = parse_sink_input_from_pairs(
        12,
        false,
        true,
        None,
        [
            ("application.name", "Discord"),
            ("media.name", "Voice Call"),
        ],
    );
    let muted_snapshot = sink_input_to_snapshot(&muted_pw);
    assert_eq!(muted_snapshot.status, PlaybackStatus::Paused);
    assert_eq!(resolve_primary_control(&muted_snapshot).tooltip, "Unmute");

    let t1 = t0 + Duration::from_millis(250);
    assert_eq!(
        machine.handle(IslandEvent::MediaUpdated(muted_snapshot), t1),
        VisibilityState::HidePending { since: t1 }
    );

    let t2 = t1 + config.hide_delay();
    assert_eq!(
        machine.handle(IslandEvent::Tick(t2), t2),
        VisibilityState::Hidden
    );
}

#[test]
fn e2e_audio_hysteresis_and_hide_pending_recovery_across_modules() {
    let config = Config::default();
    let mut analyzer = SpectrumAnalyzer::new(config.bar_count);
    let mut machine = IslandStateMachine::new(config.hide_delay());

    let t0 = Instant::now();

    // 1. Deadband amplitude (0.002 between AUDIBLE_OFF_RMS=0.001 and AUDIBLE_ON_RMS=0.003) stays hidden.
    let deadband = vec![0.002_f32; FFT_SIZE];
    let f_deadband = analyzer.process(&deadband);
    assert!(!f_deadband.is_audible);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(f_deadband.is_audible), t0),
        VisibilityState::Hidden
    );

    // 2. Crossing above AUDIBLE_ON_RMS reveals the pill.
    let t1 = t0 + Duration::from_millis(100);
    let loud = sine_wave(1_000.0, 0.4, FFT_SIZE);
    let f_loud = analyzer.process(&loud);
    assert!(f_loud.is_audible);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(f_loud.is_audible), t1),
        VisibilityState::VisibleCompact
    );

    // 3. Dropping back into the deadband preserves audibility and avoids HidePending flicker.
    let t2 = t1 + Duration::from_millis(100);
    let f_still_audible = analyzer.process(&deadband);
    assert!(f_still_audible.is_audible);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(f_still_audible.is_audible), t2),
        VisibilityState::VisibleCompact
    );

    // 4. Dropping below AUDIBLE_OFF_RMS starts HidePending.
    let t3 = t2 + Duration::from_millis(100);
    let quiet = vec![0.0004_f32; FFT_SIZE];
    let f_quiet = analyzer.process(&quiet);
    assert!(!f_quiet.is_audible);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(f_quiet.is_audible), t3),
        VisibilityState::HidePending { since: t3 }
    );

    // Repeated silent frames and paused media updates must not reset HidePending timestamp.
    let t4 = t3 + Duration::from_millis(1500);
    let f_quiet_2 = analyzer.process(&quiet);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(f_quiet_2.is_audible), t4),
        VisibilityState::HidePending { since: t3 }
    );
    assert_eq!(
        machine.handle(
            IslandEvent::MediaUpdated(paused_mpris_snapshot("Spotify", "Song", "Artist")),
            t4
        ),
        VisibilityState::HidePending { since: t3 }
    );

    // 5. Resuming audio before hide_delay cancels HidePending and restores VisibleCompact.
    let t5 = t3 + Duration::from_millis(2500);
    let f_resumed = analyzer.process(&loud);
    assert!(f_resumed.is_audible);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(f_resumed.is_audible), t5),
        VisibilityState::VisibleCompact
    );

    // Advancing past the original t3 + hide_delay keeps the island visible.
    let t6 = t3 + config.hide_delay() + Duration::from_secs(2);
    assert_eq!(
        machine.handle(IslandEvent::Tick(t6), t6),
        VisibilityState::VisibleCompact
    );
}

#[test]
fn e2e_config_clamping_and_non_finite_samples_across_pipeline() {
    let high_cfg = Config::parse_toml(
        r#"
        bar_count = 128
        accent_color = "not-a-color"
        "#,
    )
    .expect("out-of-range bar_count should still parse and clamp");
    assert_eq!(high_cfg.bar_count, MAX_BAR_COUNT);

    let low_cfg =
        Config::parse_toml("bar_count = 1").expect("low bar_count should parse and clamp");
    assert_eq!(low_cfg.bar_count, MIN_BAR_COUNT);

    let mut analyzer = SpectrumAnalyzer::new(high_cfg.bar_count);
    let mut equalizer = EqualizerState::new(high_cfg.bar_count, &high_cfg.accent_color);
    assert_eq!(equalizer.color(), RgbColor::default());

    equalizer.set_active(true);

    let malformed_samples = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.25, -0.25];
    let frame = analyzer.process(&malformed_samples);
    assert_eq!(frame.bars.len(), MAX_BAR_COUNT);
    assert!(frame.rms.is_finite());
    assert!(
        frame
            .bars
            .iter()
            .all(|&b| b.is_finite() && (0.0..=1.0).contains(&b))
    );

    equalizer.update_frame(&frame, false);
    let _ = equalizer.tick();
    assert_eq!(equalizer.displayed_bars().len(), MAX_BAR_COUNT);
    assert!(
        equalizer
            .displayed_bars()
            .iter()
            .all(|&b| b.is_finite() && (0.0..=1.0).contains(&b))
    );
}

#[test]
fn e2e_config_load_from_disk_drives_window_placement_and_expanded_layout() {
    let cfg_file = TempConfigFile::new("layout.toml");
    std::fs::write(
        &cfg_file.path,
        r##"
        corner = "top-left"
        margin_x = 18
        margin_y = 42
        hide_delay_ms = 2500
        bar_count = 10
        accent_color = "#9ece6a"
        "##,
    )
    .expect("should write temporary config file");

    let config = Config::load_from_path(&cfg_file.path);
    assert_eq!(config.corner, Corner::TopLeft);
    assert_eq!(config.bar_count, 10);

    let placement = layer_placement(config.corner, config.margin_x, config.margin_y);
    assert_eq!(placement.vertical_edge, Edge::Top);
    assert_eq!(placement.horizontal_edge, Edge::Left);
    assert_eq!(placement.vertical_margin, 42);
    assert_eq!(placement.horizontal_margin, 18);

    let (transition, expanded_before_compact) = expanded_layout(config.corner);
    assert_eq!(transition, RevealerTransitionType::SlideRight);
    assert!(!expanded_before_compact);

    let analyzer = SpectrumAnalyzer::new(config.bar_count);
    let equalizer = EqualizerState::new(config.bar_count, &config.accent_color);
    assert_eq!(analyzer.bar_count(), 10);
    assert_eq!(equalizer.displayed_bars().len(), 10);
    assert_eq!(equalizer.color(), RgbColor::from_u8(0x9e, 0xce, 0x6a));
}

#[test]
fn e2e_set_enabled_hides_immediately_blocks_reappearance_and_restores_with_active_audio() {
    let config = Config::parse_toml(
        r#"
        hide_delay_ms = 1500
        bar_count = 8
        "#,
    )
    .expect("valid TOML config should parse");

    let mut analyzer = SpectrumAnalyzer::new(config.bar_count);
    let mut machine = IslandStateMachine::new(config.hide_delay());
    let mut equalizer = EqualizerState::new(config.bar_count, &config.accent_color);

    let t0 = Instant::now();
    let audible_pcm = sine_wave(440.0, 0.7, FFT_SIZE);
    let silent_pcm = vec![0.0_f32; FFT_SIZE];

    // 1. Activate audio, playing media, hover, and drag so all active flags are engaged.
    let frame = analyzer.process(&audible_pcm);
    assert!(frame.is_audible);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(frame.is_audible), t0),
        VisibilityState::VisibleCompact
    );
    let playing = playing_mpris_snapshot("Spotify", "Resonance", "HOME");
    assert_eq!(
        machine.handle(IslandEvent::MediaUpdated(playing.clone()), t0),
        VisibilityState::VisibleCompact
    );
    assert_eq!(
        machine.handle(IslandEvent::PointerEntered, t0),
        VisibilityState::VisibleExpanded
    );
    assert_eq!(
        machine.handle(IslandEvent::DraggingChanged(true), t0),
        VisibilityState::VisibleExpanded
    );

    equalizer.set_active(machine.is_visible());
    equalizer.update_frame(&frame, false);
    assert!(equalizer.tick());

    // 2. Disabling must immediately hide the island and reset dragging even while audio/media play.
    let t1 = t0 + Duration::from_millis(100);
    assert_eq!(
        machine.handle(IslandEvent::SetEnabled(false), t1),
        VisibilityState::Hidden
    );
    assert!(!machine.is_visible());
    assert!(!machine.is_enabled());
    assert!(!machine.is_dragging());

    equalizer.set_active(machine.is_visible());
    assert!(!equalizer.is_active());

    // 3. Reappearance must stay blocked across audio spikes, media updates, hover, drag, and ticks.
    let t2 = t1 + Duration::from_millis(200);
    let spike_frame = analyzer.process(&audible_pcm);
    assert!(spike_frame.is_audible);
    for event in [
        IslandEvent::AudioActivity(false),
        IslandEvent::AudioActivity(spike_frame.is_audible),
        IslandEvent::MediaUpdated(playing.clone()),
        IslandEvent::PointerEntered,
        IslandEvent::DraggingChanged(true),
        IslandEvent::Tick(t2 + Duration::from_secs(5)),
    ] {
        assert_eq!(machine.handle(event, t2), VisibilityState::Hidden);
        assert!(!machine.is_visible());
        assert!(!machine.is_dragging());
    }

    // 4. Re-enabling while audio/media are still active restores visibility immediately.
    let t3 = t2 + Duration::from_millis(50);
    assert_eq!(
        machine.handle(IslandEvent::SetEnabled(true), t3),
        VisibilityState::VisibleExpanded
    );
    assert!(machine.is_enabled());
    assert!(machine.is_visible());

    assert_eq!(
        machine.handle(IslandEvent::PointerLeft, t3),
        VisibilityState::VisibleCompact
    );

    // 5. If audio and media stop while disabled, re-enabling stays hidden until new audio arrives.
    let t4 = t3 + Duration::from_millis(100);
    assert_eq!(
        machine.handle(IslandEvent::SetEnabled(false), t4),
        VisibilityState::Hidden
    );
    let silent_frame = analyzer.process(&silent_pcm);
    assert!(!silent_frame.is_audible);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(silent_frame.is_audible), t4),
        VisibilityState::Hidden
    );
    assert_eq!(
        machine.handle(
            IslandEvent::MediaUpdated(paused_mpris_snapshot("Spotify", "Resonance", "HOME")),
            t4
        ),
        VisibilityState::Hidden
    );
    assert_eq!(
        machine.handle(IslandEvent::SetEnabled(true), t4),
        VisibilityState::Hidden
    );

    let t5 = t4 + Duration::from_millis(100);
    let resumed_frame = analyzer.process(&audible_pcm);
    assert!(resumed_frame.is_audible);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(resumed_frame.is_audible), t5),
        VisibilityState::VisibleCompact
    );
}

#[test]
fn e2e_dragging_prevents_hide_during_hide_pending_and_pointer_left_then_resumes_normal_cycle() {
    let config = Config::parse_toml("hide_delay_ms = 1000").expect("valid TOML config");
    let mut analyzer = SpectrumAnalyzer::new(config.bar_count);
    let mut machine = IslandStateMachine::new(config.hide_delay());
    let mut equalizer = EqualizerState::new(config.bar_count, &config.accent_color);

    let t0 = Instant::now();
    let audible_pcm = sine_wave(660.0, 0.6, FFT_SIZE);
    let silent_pcm = vec![0.0_f32; FFT_SIZE];

    // 1. Bring capsule to VisibleExpanded, then drop audio and leave pointer to enter HidePending.
    let audible_frame = analyzer.process(&audible_pcm);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(audible_frame.is_audible), t0),
        VisibilityState::VisibleCompact
    );
    assert_eq!(
        machine.handle(IslandEvent::PointerEntered, t0),
        VisibilityState::VisibleExpanded
    );
    equalizer.set_active(machine.is_visible());

    let t_silent = t0 + Duration::from_millis(100);
    let silent_frame = analyzer.process(&silent_pcm);
    assert!(!silent_frame.is_audible);
    assert_eq!(
        machine.handle(
            IslandEvent::AudioActivity(silent_frame.is_audible),
            t_silent
        ),
        VisibilityState::VisibleExpanded
    );

    let t_leave = t_silent + Duration::from_millis(50);
    assert_eq!(
        machine.handle(IslandEvent::PointerLeft, t_leave),
        VisibilityState::HidePending { since: t_leave }
    );

    // 2. Starting drag during HidePending exits HidePending and prevents hiding past hide_delay.
    let t_drag = t_leave + Duration::from_millis(500);
    assert_eq!(
        machine.handle(IslandEvent::DraggingChanged(true), t_drag),
        VisibilityState::VisibleCompact
    );
    assert!(machine.is_dragging());
    assert!(machine.is_visible());

    let t_long_drag = t_drag + config.hide_delay() + Duration::from_secs(5);
    assert_eq!(
        machine.handle(IslandEvent::Tick(t_long_drag), t_long_drag),
        VisibilityState::VisibleCompact
    );

    // 3. PointerEntered + PointerLeft while dragging (e.g., cursor outrunning margin updates)
    // must keep the capsule visible and not start HidePending until drag ends.
    assert_eq!(
        machine.handle(IslandEvent::PointerEntered, t_long_drag),
        VisibilityState::VisibleExpanded
    );
    assert_eq!(
        machine.handle(IslandEvent::PointerLeft, t_long_drag),
        VisibilityState::VisibleExpanded
    );
    assert!(machine.is_dragging());
    assert!(machine.is_visible());

    let t_still_dragging = t_long_drag + config.hide_delay() + Duration::from_secs(2);
    assert_eq!(
        machine.handle(IslandEvent::Tick(t_still_dragging), t_still_dragging),
        VisibilityState::VisibleExpanded
    );

    // 4. Releasing drag while silent and unhovered resumes the normal HidePending -> Hidden cycle.
    let t_drop = t_still_dragging + Duration::from_millis(100);
    assert_eq!(
        machine.handle(IslandEvent::DraggingChanged(false), t_drop),
        VisibilityState::HidePending { since: t_drop }
    );
    assert!(!machine.is_dragging());
    assert!(machine.is_visible());

    let t_just_before = t_drop + config.hide_delay() - Duration::from_millis(1);
    assert_eq!(
        machine.handle(IslandEvent::Tick(t_just_before), t_just_before),
        VisibilityState::HidePending { since: t_drop }
    );

    let t_expired = t_drop + config.hide_delay();
    assert_eq!(
        machine.handle(IslandEvent::Tick(t_expired), t_expired),
        VisibilityState::Hidden
    );
    assert!(!machine.is_visible());
    equalizer.set_active(machine.is_visible());
    assert!(!equalizer.is_active());

    // 5. Releasing drag while audio is active resumes VisibleCompact without hiding.
    let t_reopen = t_expired + Duration::from_millis(100);
    let reopened = analyzer.process(&audible_pcm);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(reopened.is_audible), t_reopen),
        VisibilityState::VisibleCompact
    );
    assert_eq!(
        machine.handle(IslandEvent::DraggingChanged(true), t_reopen),
        VisibilityState::VisibleCompact
    );
    assert_eq!(
        machine.handle(IslandEvent::PointerLeft, t_reopen),
        VisibilityState::VisibleCompact
    );
    assert_eq!(
        machine.handle(IslandEvent::DraggingChanged(false), t_reopen),
        VisibilityState::VisibleCompact
    );
    assert_eq!(
        machine.handle(
            IslandEvent::Tick(t_reopen + config.hide_delay() + Duration::from_secs(1)),
            t_reopen + config.hide_delay() + Duration::from_secs(1)
        ),
        VisibilityState::VisibleCompact
    );
}

#[test]
fn e2e_drag_delta_persists_through_config_save_and_load_for_all_corners() {
    struct CornerCase {
        name: &'static str,
        corner: Corner,
        initial: (i32, i32),
        delta: (i32, i32),
        max_bounds: (i32, i32),
        expected: (i32, i32),
        clamped_zero_delta: (i32, i32),
        v_edge: Edge,
        h_edge: Edge,
    }

    let cases = [
        CornerCase {
            name: "top-right",
            corner: Corner::TopRight,
            initial: (40, 60),
            delta: (-35, 25),
            max_bounds: (1920, 1080),
            expected: (75, 85),
            clamped_zero_delta: (500, -500),
            v_edge: Edge::Top,
            h_edge: Edge::Right,
        },
        CornerCase {
            name: "top-left",
            corner: Corner::TopLeft,
            initial: (16, 48),
            delta: (24, -18),
            max_bounds: (1920, 1080),
            expected: (40, 30),
            clamped_zero_delta: (-500, -500),
            v_edge: Edge::Top,
            h_edge: Edge::Left,
        },
        CornerCase {
            name: "bottom-right",
            corner: Corner::BottomRight,
            initial: (20, 30),
            delta: (-30, -45),
            max_bounds: (1920, 1080),
            expected: (50, 75),
            clamped_zero_delta: (500, 500),
            v_edge: Edge::Bottom,
            h_edge: Edge::Right,
        },
        CornerCase {
            name: "bottom-left",
            corner: Corner::BottomLeft,
            initial: (10, 20),
            delta: (5000, -5000),
            max_bounds: (800, 600),
            expected: (800, 600),
            clamped_zero_delta: (-5000, 5000),
            v_edge: Edge::Bottom,
            h_edge: Edge::Left,
        },
    ];

    let temp_root = TempDir::new("drag_roundtrip");

    for case in cases {
        let mut config = Config {
            corner: case.corner,
            margin_x: case.initial.0,
            margin_y: case.initial.1,
            hide_delay_ms: 2200,
            bar_count: 16,
            accent_color: "#f7768e".to_owned(),
            ignore_apps: vec!["zen".to_owned(), "vlc".to_owned()],
        };

        let (next_x, next_y) = apply_drag_delta(
            config.corner,
            (config.margin_x, config.margin_y),
            case.delta,
            case.max_bounds,
        );
        assert_eq!((next_x, next_y), case.expected, "corner {}", case.name);

        config.margin_x = next_x;
        config.margin_y = next_y;

        let config_path = temp_root.path.join(case.name).join("config.toml");
        config
            .save_to_path(&config_path)
            .expect("config.save_to_path should create parent dirs and persist TOML");

        let loaded = Config::load_from_path(&config_path);
        assert_eq!(loaded, config, "round-trip mismatch for {}", case.name);

        let placement = layer_placement(loaded.corner, loaded.margin_x, loaded.margin_y);
        assert_eq!(placement.vertical_edge, case.v_edge);
        assert_eq!(placement.horizontal_edge, case.h_edge);
        assert_eq!(placement.horizontal_margin, case.expected.0);
        assert_eq!(placement.vertical_margin, case.expected.1);

        // Also verify clamping to 0 persists cleanly across a second save/load pass.
        let zeroed = apply_drag_delta(
            loaded.corner,
            (loaded.margin_x, loaded.margin_y),
            case.clamped_zero_delta,
            case.max_bounds,
        );
        assert_eq!(zeroed, (0, 0), "zero clamp mismatch for {}", case.name);

        let mut reset_cfg = loaded;
        reset_cfg.margin_x = zeroed.0;
        reset_cfg.margin_y = zeroed.1;
        reset_cfg
            .save_to_path(&config_path)
            .expect("overwrite save_to_path should succeed");
        let reloaded = Config::load_from_path(&config_path);
        assert_eq!((reloaded.margin_x, reloaded.margin_y), (0, 0));
    }
}

#[test]
fn e2e_ipc_server_commands_status_waybar_json_and_state_machine_flow() {
    let sock_file = TempConfigFile::new("ipc_e2e.sock");
    let sock_path = sock_file.path.clone();

    // 1. Before daemon starts, status is Stopped and Waybar JSON reflects stopped state.
    let stopped_status = query_status_at(&sock_path);
    assert_eq!(stopped_status, DaemonStatus::Stopped);
    let stopped_json = format_waybar_json(stopped_status);
    assert!(!stopped_json.contains('\n'));
    assert!(stopped_json.starts_with('{') && stopped_json.ends_with('}'));
    assert!(stopped_json.contains("\"class\":\"stopped\""));
    assert!(stopped_json.contains("\"text\":"));
    assert!(stopped_json.contains("\"tooltip\":"));

    // 2. Start IPC server with daemon enabled and an active audio stream driving the island.
    let enabled = Arc::new(AtomicBool::new(true));
    let (server, rx) = IpcServer::start_at(sock_path.clone(), Arc::clone(&enabled));
    let mut server = server.expect("IpcServer::start_at should bind socket");

    let mut analyzer = SpectrumAnalyzer::new(12);
    let mut machine = IslandStateMachine::new(Duration::from_millis(2000));
    let t0 = Instant::now();
    let frame = analyzer.process(&sine_wave(440.0, 0.7, FFT_SIZE));
    assert!(frame.is_audible);
    assert_eq!(
        machine.handle(IslandEvent::AudioActivity(frame.is_audible), t0),
        VisibilityState::VisibleCompact
    );

    let initial_status = query_status_at(&sock_path);
    assert_eq!(initial_status, DaemonStatus::Enabled);
    let enabled_json = format_waybar_json(initial_status);
    assert!(!enabled_json.contains('\n'));
    assert!(enabled_json.contains("\"class\":\"enabled\""));

    // 3. Disable via IPC -> updates status, Waybar JSON, and hides the state machine immediately.
    assert_eq!(
        send_command_to_path(&sock_path, IpcCommand::Disable)
            .expect("disable command should succeed"),
        "disabled"
    );
    let evt = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("expected SetEnabled(false) IPC event");
    assert_eq!(evt, IpcEvent::SetEnabled(false));
    if let IpcEvent::SetEnabled(flag) = evt {
        assert_eq!(
            machine.handle(
                IslandEvent::SetEnabled(flag),
                t0 + Duration::from_millis(50)
            ),
            VisibilityState::Hidden
        );
    }

    let disabled_status = query_status_at(&sock_path);
    assert_eq!(disabled_status, DaemonStatus::Disabled);
    let disabled_json = format_waybar_json(disabled_status);
    assert!(!disabled_json.contains('\n'));
    assert!(disabled_json.contains("\"class\":\"disabled\""));

    // 4. Toggle via IPC -> re-enables and restores island visibility since audio is active.
    assert_eq!(
        send_command_to_path(&sock_path, IpcCommand::Toggle)
            .expect("toggle command should succeed"),
        "enabled"
    );
    let evt = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("expected SetEnabled(true) IPC event");
    assert_eq!(evt, IpcEvent::SetEnabled(true));
    if let IpcEvent::SetEnabled(flag) = evt {
        assert_eq!(
            machine.handle(
                IslandEvent::SetEnabled(flag),
                t0 + Duration::from_millis(100)
            ),
            VisibilityState::VisibleCompact
        );
    }
    assert_eq!(query_status_at(&sock_path), DaemonStatus::Enabled);

    // 5. Toggle back off, then explicit Enable, then ResetPos.
    assert_eq!(
        send_command_to_path(&sock_path, IpcCommand::Toggle).expect("second toggle should succeed"),
        "disabled"
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        IpcEvent::SetEnabled(false)
    );
    assert_eq!(
        send_command_to_path(&sock_path, IpcCommand::Enable)
            .expect("enable command should succeed"),
        "enabled"
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        IpcEvent::SetEnabled(true)
    );
    assert_eq!(
        send_command_to_path(&sock_path, IpcCommand::ResetPos)
            .expect("reset-pos command should succeed"),
        "ok"
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        IpcEvent::ResetPosition
    );
    assert_eq!(
        send_command_to_path(&sock_path, IpcCommand::Status)
            .expect("status command should succeed"),
        "enabled"
    );

    // 6. Stopping the server cleans up the socket and restores Stopped Waybar JSON.
    server.stop();
    assert!(!sock_path.exists());
    let final_status = query_status_at(&sock_path);
    assert_eq!(final_status, DaemonStatus::Stopped);
    assert!(format_waybar_json(final_status).contains("\"class\":\"stopped\""));
}
