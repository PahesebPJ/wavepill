//! Dynamic Island capsule widget with compact visualizer and hover-expanded media controls.

use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;

use gtk4::gdk::prelude::GdkCairoContextExt;
use gtk4::{gdk_pixbuf, gio, pango, prelude::*};

use crate::audio::SpectrumFrame;
use crate::config::{Config, Corner};
use crate::media::{ControlMode, MediaCommand, MediaSnapshot, PlaybackStatus};

use super::equalizer::EqualizerWidget;

const EXPAND_TRANSITION_MS: u32 = 220;
const ART_SIZE_PX: i32 = 26;
const TITLE_MAX_CHARS: i32 = 22;

/// Resolved appearance and behavior for the primary action button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrimaryControl {
    pub icon_name: &'static str,
    pub command: MediaCommand,
    pub sensitive: bool,
    pub tooltip: &'static str,
}

/// Resolves the primary button icon, command, sensitivity, and tooltip from media state.
pub fn resolve_primary_control(snapshot: &MediaSnapshot) -> PrimaryControl {
    match snapshot.control_mode {
        ControlMode::PipeWireMute => {
            if snapshot.is_muted {
                PrimaryControl {
                    icon_name: "audio-volume-muted-symbolic",
                    command: MediaCommand::ToggleMute,
                    sensitive: true,
                    tooltip: "Unmute",
                }
            } else {
                PrimaryControl {
                    icon_name: "audio-volume-high-symbolic",
                    command: MediaCommand::ToggleMute,
                    sensitive: true,
                    tooltip: "Mute",
                }
            }
        }
        ControlMode::Mpris => {
            if snapshot.status == PlaybackStatus::Playing {
                PrimaryControl {
                    icon_name: "media-playback-pause-symbolic",
                    command: MediaCommand::PlayPause,
                    sensitive: true,
                    tooltip: "Pause",
                }
            } else {
                PrimaryControl {
                    icon_name: "media-playback-start-symbolic",
                    command: MediaCommand::PlayPause,
                    sensitive: true,
                    tooltip: "Play",
                }
            }
        }
        ControlMode::None => PrimaryControl {
            icon_name: "media-playback-start-symbolic",
            command: MediaCommand::PlayPause,
            sensitive: false,
            tooltip: "No active player",
        },
    }
}

/// Formats `(title, subtitle)` strings for the expanded metadata column.
pub fn format_media_labels(snapshot: &MediaSnapshot) -> (String, String) {
    let title = snapshot.title.trim();
    let artist = snapshot.artist.trim();
    let app = snapshot.app_name.trim();

    if !title.is_empty() {
        let subtitle = if !artist.is_empty() {
            artist.to_owned()
        } else if !app.is_empty() && !app.eq_ignore_ascii_case(title) {
            app.to_owned()
        } else {
            String::new()
        };
        (title.to_owned(), subtitle)
    } else if !app.is_empty() {
        (app.to_owned(), artist.to_owned())
    } else {
        ("Audio Playing".to_owned(), artist.to_owned())
    }
}

/// Determines the revealer slide transition direction and child ordering for a corner.
///
/// Returns `(transition_type, expanded_before_compact)`.
pub fn expanded_layout(corner: Corner) -> (gtk4::RevealerTransitionType, bool) {
    match corner {
        Corner::TopRight | Corner::BottomRight => (gtk4::RevealerTransitionType::SlideLeft, true),
        Corner::TopLeft | Corner::BottomLeft => (gtk4::RevealerTransitionType::SlideRight, false),
    }
}

/// Returns `true` when `url` uses a local `file://` URI scheme.
pub fn is_local_file_uri(url: &str) -> bool {
    url.trim().starts_with("file://")
}

fn fallback_icon_name(snapshot: &MediaSnapshot) -> &'static str {
    match snapshot.control_mode {
        ControlMode::Mpris => "multimedia-player-symbolic",
        ControlMode::PipeWireMute | ControlMode::None => "audio-x-generic-symbolic",
    }
}

/// Dynamic Island capsule widget housing compact visualizer + controls and hover-revealed metadata.
#[derive(Clone)]
pub struct IslandWidget {
    capsule: gtk4::Box,
    expanded_revealer: gtk4::Revealer,
    equalizer: EqualizerWidget,
    art_stack: gtk4::Stack,
    art_icon: gtk4::Image,
    art_canvas: gtk4::DrawingArea,
    art_pixbuf: Rc<RefCell<Option<gdk_pixbuf::Pixbuf>>>,
    title_label: gtk4::Label,
    subtitle_label: gtk4::Label,
    prev_btn: gtk4::Button,
    play_btn: gtk4::Button,
    next_btn: gtk4::Button,
    primary_command: Rc<Cell<MediaCommand>>,
    last_art_url: Rc<RefCell<Option<String>>>,
}

impl IslandWidget {
    /// Builds the Dynamic Island capsule widget configured for `config`.
    pub fn new(
        config: &Config,
        on_hover: Rc<dyn Fn(bool)>,
        on_command: Rc<dyn Fn(MediaCommand)>,
    ) -> Self {
        let capsule = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        capsule.set_valign(gtk4::Align::Center);
        capsule.add_css_class("wavepill-capsule");

        // Compact zone (always visible whenever the pill is shown).
        let compact_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        compact_box.set_valign(gtk4::Align::Center);
        compact_box.add_css_class("wavepill-compact");

        let art_stack = gtk4::Stack::new();
        art_stack.set_size_request(ART_SIZE_PX, ART_SIZE_PX);
        art_stack.set_halign(gtk4::Align::Center);
        art_stack.set_valign(gtk4::Align::Center);
        art_stack.set_overflow(gtk4::Overflow::Hidden);
        art_stack.add_css_class("wavepill-art");

        let art_icon = gtk4::Image::from_icon_name("audio-x-generic-symbolic");
        art_icon.set_pixel_size(14);
        art_icon.set_halign(gtk4::Align::Center);
        art_icon.set_valign(gtk4::Align::Center);

        let art_pixbuf: Rc<RefCell<Option<gdk_pixbuf::Pixbuf>>> = Rc::new(RefCell::new(None));
        let art_canvas = gtk4::DrawingArea::new();
        art_canvas.set_content_width(ART_SIZE_PX);
        art_canvas.set_content_height(ART_SIZE_PX);
        art_canvas.set_halign(gtk4::Align::Center);
        art_canvas.set_valign(gtk4::Align::Center);
        {
            let pixbuf_ref = Rc::clone(&art_pixbuf);
            art_canvas.set_draw_func(move |_area, cr, width, height| {
                let w = f64::from(width);
                let h = f64::from(height);
                if w <= 0.0 || h <= 0.0 {
                    return;
                }
                let Some(ref pixbuf) = *pixbuf_ref.borrow() else {
                    return;
                };
                let pw = f64::from(pixbuf.width().max(1));
                let ph = f64::from(pixbuf.height().max(1));
                let scale = (w / pw).max(h / ph);
                let ox = (w - pw * scale) * 0.5;
                let oy = (h - ph * scale) * 0.5;

                cr.arc(w * 0.5, h * 0.5, w.min(h) * 0.5, 0.0, 2.0 * PI);
                cr.clip();
                cr.translate(ox, oy);
                cr.scale(scale, scale);
                cr.set_source_pixbuf(pixbuf, 0.0, 0.0);
                let _ = cr.paint();
            });
        }

        art_stack.add_named(&art_icon, Some("icon"));
        art_stack.add_named(&art_canvas, Some("art"));
        art_stack.set_visible_child_name("icon");

        let equalizer = EqualizerWidget::new(config.bar_count, &config.accent_color);

        let primary_command = Rc::new(Cell::new(MediaCommand::PlayPause));
        let play_btn = build_action_button("media-playback-start-symbolic", "Play");
        play_btn.add_css_class("wavepill-primary-btn");
        {
            let cmd_cell = Rc::clone(&primary_command);
            let cmd_cb = Rc::clone(&on_command);
            play_btn.connect_clicked(move |_| {
                cmd_cb(cmd_cell.get());
            });
        }

        compact_box.append(&art_stack);
        compact_box.append(equalizer.widget());
        compact_box.append(&play_btn);

        // Expanded zone (revealed on pointer hover).
        let (transition_type, expanded_first) = expanded_layout(config.corner);
        let expanded_revealer = gtk4::Revealer::new();
        expanded_revealer.set_transition_type(transition_type);
        expanded_revealer.set_transition_duration(EXPAND_TRANSITION_MS);
        expanded_revealer.set_reveal_child(false);

        let expanded_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        expanded_box.set_valign(gtk4::Align::Center);
        expanded_box.add_css_class("wavepill-expanded");
        if expanded_first {
            expanded_box.set_margin_end(10);
        } else {
            expanded_box.set_margin_start(10);
        }

        let meta_box = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
        meta_box.set_valign(gtk4::Align::Center);
        meta_box.add_css_class("wavepill-meta");

        let title_label = gtk4::Label::new(Some("Audio Playing"));
        title_label.set_ellipsize(pango::EllipsizeMode::End);
        title_label.set_max_width_chars(TITLE_MAX_CHARS);
        title_label.set_xalign(0.0);
        title_label.set_single_line_mode(true);
        title_label.add_css_class("wavepill-title");

        let subtitle_label = gtk4::Label::new(None);
        subtitle_label.set_ellipsize(pango::EllipsizeMode::End);
        subtitle_label.set_max_width_chars(TITLE_MAX_CHARS);
        subtitle_label.set_xalign(0.0);
        subtitle_label.set_single_line_mode(true);
        subtitle_label.set_visible(false);
        subtitle_label.add_css_class("wavepill-subtitle");

        meta_box.append(&title_label);
        meta_box.append(&subtitle_label);

        let prev_btn = build_action_button("media-skip-backward-symbolic", "Previous");
        {
            let cmd_cb = Rc::clone(&on_command);
            prev_btn.connect_clicked(move |_| cmd_cb(MediaCommand::Previous));
        }

        let next_btn = build_action_button("media-skip-forward-symbolic", "Next");
        {
            let cmd_cb = Rc::clone(&on_command);
            next_btn.connect_clicked(move |_| cmd_cb(MediaCommand::Next));
        }

        expanded_box.append(&meta_box);
        expanded_box.append(&prev_btn);
        expanded_box.append(&next_btn);
        expanded_revealer.set_child(Some(&expanded_box));

        if expanded_first {
            capsule.append(&expanded_revealer);
            capsule.append(&compact_box);
        } else {
            capsule.append(&compact_box);
            capsule.append(&expanded_revealer);
        }

        let motion = gtk4::EventControllerMotion::new();
        {
            let hover_enter = Rc::clone(&on_hover);
            motion.connect_enter(move |_, _, _| hover_enter(true));
        }
        {
            let hover_leave = on_hover;
            motion.connect_leave(move |_| hover_leave(false));
        }
        capsule.add_controller(motion);

        let widget = Self {
            capsule,
            expanded_revealer,
            equalizer,
            art_stack,
            art_icon,
            art_canvas,
            art_pixbuf,
            title_label,
            subtitle_label,
            prev_btn,
            play_btn,
            next_btn,
            primary_command,
            last_art_url: Rc::new(RefCell::new(None)),
        };

        widget.update_media(&MediaSnapshot::default());
        widget
    }

    /// Returns the root capsule container widget.
    pub fn widget(&self) -> &gtk4::Box {
        &self.capsule
    }

    /// Expands or collapses the secondary media info and skip controls.
    pub fn set_expanded(&self, expanded: bool) {
        if self.expanded_revealer.reveals_child() != expanded {
            self.expanded_revealer.set_reveal_child(expanded);
        }
    }

    /// Enables or pauses the equalizer animation loop according to visibility.
    pub fn set_active(&self, active: bool) {
        self.equalizer.set_active(active);
    }

    /// Updates the spectrum bars from the latest [`SpectrumFrame`].
    pub fn update_spectrum(&self, frame: &SpectrumFrame, is_paused: bool) {
        self.equalizer.update_frame(frame, is_paused);
    }

    /// Synchronizes track metadata, cover art, and media control states.
    pub fn update_media(&self, snapshot: &MediaSnapshot) {
        let (title, subtitle) = format_media_labels(snapshot);
        self.title_label.set_text(&title);
        if subtitle.is_empty() {
            self.subtitle_label.set_visible(false);
        } else {
            self.subtitle_label.set_text(&subtitle);
            self.subtitle_label.set_visible(true);
        }

        let primary = resolve_primary_control(snapshot);
        self.primary_command.set(primary.command);
        self.play_btn.set_icon_name(primary.icon_name);
        self.play_btn.set_sensitive(primary.sensitive);
        self.play_btn.set_tooltip_text(Some(primary.tooltip));

        let is_mpris = snapshot.control_mode == ControlMode::Mpris;
        self.prev_btn.set_visible(is_mpris);
        self.prev_btn
            .set_sensitive(is_mpris && snapshot.can_go_previous);
        self.next_btn.set_visible(is_mpris);
        self.next_btn
            .set_sensitive(is_mpris && snapshot.can_go_next);

        self.art_icon
            .set_icon_name(Some(fallback_icon_name(snapshot)));
        self.sync_album_art(snapshot.art_url.as_deref());
    }

    fn sync_album_art(&self, art_url: Option<&str>) {
        let normalized = art_url.map(str::trim).filter(|s| !s.is_empty());
        let unchanged = self.last_art_url.borrow().as_deref() == normalized;
        if unchanged {
            return;
        }

        *self.last_art_url.borrow_mut() = normalized.map(str::to_owned);

        if let Some(uri) = normalized
            && is_local_file_uri(uri)
            && let Some(pixbuf) = load_local_pixbuf(uri)
        {
            *self.art_pixbuf.borrow_mut() = Some(pixbuf);
            self.art_canvas.queue_draw();
            self.art_stack.set_visible_child_name("art");
            return;
        }

        *self.art_pixbuf.borrow_mut() = None;
        self.art_stack.set_visible_child_name("icon");
    }
}

fn build_action_button(icon_name: &str, tooltip: &str) -> gtk4::Button {
    let button = gtk4::Button::from_icon_name(icon_name);
    button.set_tooltip_text(Some(tooltip));
    button.set_valign(gtk4::Align::Center);
    button.set_can_focus(false);
    button.set_focus_on_click(false);
    button.add_css_class("wavepill-btn");
    button
}

fn load_local_pixbuf(uri: &str) -> Option<gdk_pixbuf::Pixbuf> {
    let file = gio::File::for_uri(uri);
    let path = file.path()?;
    if !path.is_file() {
        return None;
    }
    gdk_pixbuf::Pixbuf::from_file_at_size(&path, 96, 96).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_primary_control_for_mpris_pipewire_and_none() {
        let playing = MediaSnapshot {
            control_mode: ControlMode::Mpris,
            status: PlaybackStatus::Playing,
            ..MediaSnapshot::default()
        };
        assert_eq!(
            resolve_primary_control(&playing),
            PrimaryControl {
                icon_name: "media-playback-pause-symbolic",
                command: MediaCommand::PlayPause,
                sensitive: true,
                tooltip: "Pause",
            }
        );

        let paused = MediaSnapshot {
            control_mode: ControlMode::Mpris,
            status: PlaybackStatus::Paused,
            ..MediaSnapshot::default()
        };
        assert_eq!(
            resolve_primary_control(&paused),
            PrimaryControl {
                icon_name: "media-playback-start-symbolic",
                command: MediaCommand::PlayPause,
                sensitive: true,
                tooltip: "Play",
            }
        );

        let muted = MediaSnapshot {
            control_mode: ControlMode::PipeWireMute,
            is_muted: true,
            ..MediaSnapshot::default()
        };
        assert_eq!(
            resolve_primary_control(&muted),
            PrimaryControl {
                icon_name: "audio-volume-muted-symbolic",
                command: MediaCommand::ToggleMute,
                sensitive: true,
                tooltip: "Unmute",
            }
        );

        let unmuted = MediaSnapshot {
            control_mode: ControlMode::PipeWireMute,
            is_muted: false,
            ..MediaSnapshot::default()
        };
        assert_eq!(
            resolve_primary_control(&unmuted),
            PrimaryControl {
                icon_name: "audio-volume-high-symbolic",
                command: MediaCommand::ToggleMute,
                sensitive: true,
                tooltip: "Mute",
            }
        );

        let none = MediaSnapshot::default();
        assert!(!resolve_primary_control(&none).sensitive);
    }

    #[test]
    fn formats_media_labels_with_fallbacks() {
        let full = MediaSnapshot {
            title: "Midnight City".to_owned(),
            artist: "M83".to_owned(),
            app_name: "Spotify".to_owned(),
            ..MediaSnapshot::default()
        };
        assert_eq!(
            format_media_labels(&full),
            ("Midnight City".to_owned(), "M83".to_owned())
        );

        let no_artist = MediaSnapshot {
            title: "Lo-fi Beats".to_owned(),
            app_name: "Firefox".to_owned(),
            ..MediaSnapshot::default()
        };
        assert_eq!(
            format_media_labels(&no_artist),
            ("Lo-fi Beats".to_owned(), "Firefox".to_owned())
        );

        let app_only = MediaSnapshot {
            app_name: "System Audio".to_owned(),
            ..MediaSnapshot::default()
        };
        assert_eq!(
            format_media_labels(&app_only),
            ("System Audio".to_owned(), String::new())
        );

        assert_eq!(
            format_media_labels(&MediaSnapshot::default()),
            ("Audio Playing".to_owned(), String::new())
        );
    }

    #[test]
    fn determines_expanded_layout_and_file_uris() {
        assert_eq!(
            expanded_layout(Corner::TopRight),
            (gtk4::RevealerTransitionType::SlideLeft, true)
        );
        assert_eq!(
            expanded_layout(Corner::BottomRight),
            (gtk4::RevealerTransitionType::SlideLeft, true)
        );
        assert_eq!(
            expanded_layout(Corner::TopLeft),
            (gtk4::RevealerTransitionType::SlideRight, false)
        );
        assert_eq!(
            expanded_layout(Corner::BottomLeft),
            (gtk4::RevealerTransitionType::SlideRight, false)
        );

        assert!(is_local_file_uri("file:///home/user/cover.png"));
        assert!(is_local_file_uri("  file:///tmp/art.jpg "));
        assert!(!is_local_file_uri("https://example.com/cover.png"));
        assert!(!is_local_file_uri(""));
    }
}
