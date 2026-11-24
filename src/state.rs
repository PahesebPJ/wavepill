//! Pure visibility and expansion state machine for the floating wavepill island.

use std::time::{Duration, Instant};

use crate::media::{MediaSnapshot, PlaybackStatus};

/// Visibility lifecycle states for the floating pill window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityState {
    Hidden,
    VisibleCompact,
    VisibleExpanded,
    HidePending { since: Instant },
}

/// Inputs driving the island state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IslandEvent {
    AudioActivity(bool),
    MediaUpdated(MediaSnapshot),
    PointerEntered,
    PointerLeft,
    SetEnabled(bool),
    DraggingChanged(bool),
    Tick(Instant),
}

/// Deterministic state machine coordinating audio activity, media playback,
/// pointer hover, drag state, enable toggle, and delayed auto-hide behavior.
#[derive(Debug, Clone)]
pub struct IslandStateMachine {
    state: VisibilityState,
    hide_delay: Duration,
    audio_active: bool,
    media_playing: bool,
    hovered: bool,
    enabled: bool,
    dragging: bool,
}

impl IslandStateMachine {
    /// Creates a new state machine starting in `VisibilityState::Hidden`.
    pub fn new(hide_delay: Duration) -> Self {
        Self {
            state: VisibilityState::Hidden,
            hide_delay,
            audio_active: false,
            media_playing: false,
            hovered: false,
            enabled: true,
            dragging: false,
        }
    }

    /// Processes an event at timestamp `now` and returns the resulting visibility state.
    pub fn handle(&mut self, event: IslandEvent, now: Instant) -> VisibilityState {
        match event {
            IslandEvent::AudioActivity(active) => {
                self.audio_active = active;
                self.reconcile_activity(now);
            }
            IslandEvent::MediaUpdated(snapshot) => {
                self.media_playing = snapshot.status == PlaybackStatus::Playing;
                self.reconcile_activity(now);
            }
            IslandEvent::PointerEntered => {
                self.hovered = true;
                if self.enabled && self.is_visible() {
                    self.state = VisibilityState::VisibleExpanded;
                }
            }
            IslandEvent::PointerLeft => {
                self.hovered = false;
                if !self.enabled {
                    self.state = VisibilityState::Hidden;
                } else if self.dragging {
                    // Keep visible while dragging; PointerLeft can fire if the pointer
                    // momentarily outruns a layer-shell margin update.
                } else {
                    match self.state {
                        VisibilityState::Hidden => {}
                        _ if self.is_active() => {
                            self.state = VisibilityState::VisibleCompact;
                        }
                        VisibilityState::HidePending { since } => {
                            self.state = VisibilityState::HidePending { since };
                        }
                        VisibilityState::VisibleCompact | VisibilityState::VisibleExpanded => {
                            self.state = VisibilityState::HidePending { since: now };
                        }
                    }
                }
            }
            IslandEvent::SetEnabled(enabled) => {
                self.enabled = enabled;
                if !enabled {
                    self.dragging = false;
                    self.state = VisibilityState::Hidden;
                } else {
                    self.reconcile_activity(now);
                }
            }
            IslandEvent::DraggingChanged(dragging) => {
                self.dragging = dragging;
                if !self.enabled {
                    self.dragging = false;
                    self.state = VisibilityState::Hidden;
                } else if !dragging {
                    self.reconcile_activity(now);
                } else if let VisibilityState::HidePending { .. } = self.state {
                    self.state = if self.hovered {
                        VisibilityState::VisibleExpanded
                    } else {
                        VisibilityState::VisibleCompact
                    };
                }
            }
            IslandEvent::Tick(tick_now) => {
                if !self.enabled {
                    self.state = VisibilityState::Hidden;
                } else if let VisibilityState::HidePending { since } = self.state
                    && !self.hovered
                    && !self.dragging
                    && !self.is_active()
                    && tick_now.saturating_duration_since(since) >= self.hide_delay
                {
                    self.state = VisibilityState::Hidden;
                }
            }
        }

        self.state
    }

    /// Returns the current visibility state.
    pub fn state(&self) -> VisibilityState {
        self.state
    }

    /// Returns `true` whenever the island should remain visible on screen
    /// (`VisibleCompact`, `VisibleExpanded`, or `HidePending`).
    pub fn is_visible(&self) -> bool {
        !matches!(self.state, VisibilityState::Hidden)
    }

    /// Returns `true` when the island is expanded to show full controls.
    pub fn is_expanded(&self) -> bool {
        matches!(self.state, VisibilityState::VisibleExpanded)
    }

    /// Returns `true` while the pointer is hovering over the island.
    pub fn is_hovered(&self) -> bool {
        self.hovered
    }

    /// Returns `true` when the overlay is enabled by the user.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Returns `true` while the user is actively dragging the island.
    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    fn is_active(&self) -> bool {
        self.audio_active || self.media_playing
    }

    fn reconcile_activity(&mut self, now: Instant) {
        if !self.enabled {
            self.state = VisibilityState::Hidden;
            return;
        }

        if self.is_active() {
            self.state = if self.hovered {
                VisibilityState::VisibleExpanded
            } else {
                VisibilityState::VisibleCompact
            };
            return;
        }

        if self.hovered || self.dragging {
            if self.is_visible() {
                self.state = if self.hovered {
                    VisibilityState::VisibleExpanded
                } else {
                    VisibilityState::VisibleCompact
                };
            }
            return;
        }

        self.state = match self.state {
            VisibilityState::Hidden => VisibilityState::Hidden,
            // Preserve the original `since` timestamp so repeated idle ticks or snapshots
            // do not indefinitely postpone hiding.
            VisibilityState::HidePending { since } => VisibilityState::HidePending { since },
            VisibilityState::VisibleCompact | VisibilityState::VisibleExpanded => {
                VisibilityState::HidePending { since: now }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_status(status: PlaybackStatus) -> MediaSnapshot {
        MediaSnapshot {
            status,
            ..MediaSnapshot::default()
        }
    }

    #[test]
    fn initial_state_is_hidden() {
        let sm = IslandStateMachine::new(Duration::from_millis(3000));
        assert_eq!(sm.state(), VisibilityState::Hidden);
        assert!(!sm.is_visible());
        assert!(!sm.is_expanded());
        assert!(!sm.is_hovered());
    }

    #[test]
    fn audio_activity_transitions_compact_pending_and_hidden() {
        let t0 = Instant::now();
        let delay = Duration::from_millis(3000);
        let mut sm = IslandStateMachine::new(delay);

        // Idle noise-off while hidden remains hidden.
        assert_eq!(
            sm.handle(IslandEvent::AudioActivity(false), t0),
            VisibilityState::Hidden
        );

        let t1 = t0 + Duration::from_millis(100);
        assert_eq!(
            sm.handle(IslandEvent::AudioActivity(true), t1),
            VisibilityState::VisibleCompact
        );
        assert!(sm.is_visible());
        assert!(!sm.is_expanded());

        let t2 = t1 + Duration::from_millis(500);
        assert_eq!(
            sm.handle(IslandEvent::AudioActivity(false), t2),
            VisibilityState::HidePending { since: t2 }
        );
        assert!(sm.is_visible());

        // Before hide_delay expires, still HidePending.
        let t3 = t2 + Duration::from_millis(2999);
        assert_eq!(
            sm.handle(IslandEvent::Tick(t3), t3),
            VisibilityState::HidePending { since: t2 }
        );
        assert!(sm.is_visible());

        // At exact hide_delay, transitions to Hidden.
        let t4 = t2 + delay;
        assert_eq!(
            sm.handle(IslandEvent::Tick(t4), t4),
            VisibilityState::Hidden
        );
        assert!(!sm.is_visible());
    }

    #[test]
    fn media_playing_keeps_visible_even_without_audio() {
        let t0 = Instant::now();
        let mut sm = IslandStateMachine::new(Duration::from_secs(3));

        assert_eq!(
            sm.handle(
                IslandEvent::MediaUpdated(snapshot_with_status(PlaybackStatus::Playing)),
                t0
            ),
            VisibilityState::VisibleCompact
        );

        // Silence while MPRIS reports Playing keeps it compact.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            sm.handle(IslandEvent::AudioActivity(false), t1),
            VisibilityState::VisibleCompact
        );

        // Once media pauses and audio is inactive, HidePending begins.
        let t2 = t1 + Duration::from_secs(1);
        assert_eq!(
            sm.handle(
                IslandEvent::MediaUpdated(snapshot_with_status(PlaybackStatus::Paused)),
                t2
            ),
            VisibilityState::HidePending { since: t2 }
        );
    }

    #[test]
    fn redundant_idle_events_preserve_hide_pending_timestamp() {
        let t0 = Instant::now();
        let delay = Duration::from_secs(3);
        let mut sm = IslandStateMachine::new(delay);

        sm.handle(IslandEvent::AudioActivity(true), t0);
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            sm.handle(IslandEvent::AudioActivity(false), t1),
            VisibilityState::HidePending { since: t1 }
        );

        let t2 = t1 + Duration::from_secs(2);
        assert_eq!(
            sm.handle(IslandEvent::AudioActivity(false), t2),
            VisibilityState::HidePending { since: t1 }
        );
        assert_eq!(
            sm.handle(
                IslandEvent::MediaUpdated(snapshot_with_status(PlaybackStatus::Stopped)),
                t2
            ),
            VisibilityState::HidePending { since: t1 }
        );

        let t3 = t1 + delay;
        assert_eq!(
            sm.handle(IslandEvent::Tick(t3), t3),
            VisibilityState::Hidden
        );
    }

    #[test]
    fn pointer_enter_expands_and_cancels_hide_pending() {
        let t0 = Instant::now();
        let delay = Duration::from_secs(3);
        let mut sm = IslandStateMachine::new(delay);

        sm.handle(IslandEvent::AudioActivity(true), t0);
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            sm.handle(IslandEvent::AudioActivity(false), t1),
            VisibilityState::HidePending { since: t1 }
        );

        let t2 = t1 + Duration::from_secs(2);
        assert_eq!(
            sm.handle(IslandEvent::PointerEntered, t2),
            VisibilityState::VisibleExpanded
        );
        assert!(sm.is_visible());
        assert!(sm.is_expanded());
        assert!(sm.is_hovered());

        // Ticking well past original hide delay does not hide while hovered.
        let t3 = t2 + Duration::from_secs(10);
        assert_eq!(
            sm.handle(IslandEvent::Tick(t3), t3),
            VisibilityState::VisibleExpanded
        );

        // Leaving while paused/silent starts a new HidePending from now.
        let t4 = t3 + Duration::from_secs(1);
        assert_eq!(
            sm.handle(IslandEvent::PointerLeft, t4),
            VisibilityState::HidePending { since: t4 }
        );
        assert!(!sm.is_expanded());
        assert!(!sm.is_hovered());

        let t5 = t4 + delay;
        assert_eq!(
            sm.handle(IslandEvent::Tick(t5), t5),
            VisibilityState::Hidden
        );
    }

    #[test]
    fn pointer_leave_returns_to_compact_when_active() {
        let t0 = Instant::now();
        let mut sm = IslandStateMachine::new(Duration::from_secs(3));

        sm.handle(
            IslandEvent::MediaUpdated(snapshot_with_status(PlaybackStatus::Playing)),
            t0,
        );
        assert_eq!(
            sm.handle(IslandEvent::PointerEntered, t0),
            VisibilityState::VisibleExpanded
        );

        // Even if audio stops, media is still Playing.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            sm.handle(IslandEvent::AudioActivity(false), t1),
            VisibilityState::VisibleExpanded
        );

        assert_eq!(
            sm.handle(IslandEvent::PointerLeft, t1),
            VisibilityState::VisibleCompact
        );
        assert!(!sm.is_expanded());
        assert!(!sm.is_hovered());
    }

    #[test]
    fn pointer_entered_while_hidden_does_not_reveal_island() {
        let t0 = Instant::now();
        let mut sm = IslandStateMachine::new(Duration::from_secs(3));

        assert_eq!(
            sm.handle(IslandEvent::PointerEntered, t0),
            VisibilityState::Hidden
        );
        assert!(!sm.is_visible());
        assert!(!sm.is_expanded());
        assert!(sm.is_hovered());

        // Becoming active while pointer is already there opens directly in expanded mode.
        assert_eq!(
            sm.handle(IslandEvent::AudioActivity(true), t0),
            VisibilityState::VisibleExpanded
        );

        // Leaving while hidden keeps hidden.
        sm.handle(IslandEvent::AudioActivity(false), t0);
        sm.handle(IslandEvent::PointerLeft, t0);
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(
            sm.handle(IslandEvent::Tick(t1), t1),
            VisibilityState::Hidden
        );
        assert_eq!(
            sm.handle(IslandEvent::PointerLeft, t1),
            VisibilityState::Hidden
        );
        assert!(!sm.is_hovered());
    }

    #[test]
    fn activity_resuming_during_hide_pending_restores_compact() {
        let t0 = Instant::now();
        let mut sm = IslandStateMachine::new(Duration::from_secs(3));

        sm.handle(IslandEvent::AudioActivity(true), t0);
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            sm.handle(IslandEvent::AudioActivity(false), t1),
            VisibilityState::HidePending { since: t1 }
        );

        let t2 = t1 + Duration::from_secs(2);
        assert_eq!(
            sm.handle(IslandEvent::AudioActivity(true), t2),
            VisibilityState::VisibleCompact
        );

        let t3 = t2 + Duration::from_secs(10);
        assert_eq!(
            sm.handle(IslandEvent::Tick(t3), t3),
            VisibilityState::VisibleCompact
        );
    }
}
