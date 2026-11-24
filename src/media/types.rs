//! Shared media contract types decoupled from MPRIS/PipeWire transport details.

/// Current playback state reported by an active media source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    #[default]
    Stopped,
}

/// Available control mode for media or audio output controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ControlMode {
    Mpris,
    PipeWireMute,
    #[default]
    None,
}

/// Snapshot of the currently tracked media session or audio output state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaSnapshot {
    pub title: String,
    pub artist: String,
    pub app_name: String,
    pub art_url: Option<String>,
    pub status: PlaybackStatus,
    pub control_mode: ControlMode,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub is_muted: bool,
}

/// Commands dispatched from the UI to the media/audio backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    PlayPause,
    Next,
    Previous,
    ToggleMute,
}
