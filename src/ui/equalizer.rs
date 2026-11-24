//! Cairo-rendered capsule spectrum equalizer widget running at ~60 FPS when visible.

use std::cell::RefCell;
use std::f64::consts::PI;
use std::rc::Rc;
use std::time::Duration;

use gtk4::{cairo, glib, prelude::*};

use crate::audio::SpectrumFrame;

const DEFAULT_RGB: (u8, u8, u8) = (0x7a, 0xa2, 0xf7);
const WIDGET_HEIGHT_PX: i32 = 24;
const FRAME_INTERVAL_MS: u64 = 16;
const ATTACK_ALPHA: f32 = 0.45;
const DECAY_ALPHA: f32 = 0.20;
const SETTLE_EPS: f32 = 1e-3;
const VERTICAL_PADDING_PX: f64 = 2.0;

/// Normalized RGB color components in `[0.0, 1.0]` for Cairo rendering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RgbColor {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

impl RgbColor {
    /// Constructs an `RgbColor` from 8-bit channel values.
    pub const fn from_u8(r: u8, g: u8, b: u8) -> Self {
        Self {
            r: r as f64 / 255.0,
            g: g as f64 / 255.0,
            b: b as f64 / 255.0,
        }
    }

    fn lightened(self, amount: f64) -> Self {
        let t = amount.clamp(0.0, 1.0);
        Self {
            r: self.r + (1.0 - self.r) * t,
            g: self.g + (1.0 - self.g) * t,
            b: self.b + (1.0 - self.b) * t,
        }
    }
}

impl Default for RgbColor {
    fn default() -> Self {
        Self::from_u8(DEFAULT_RGB.0, DEFAULT_RGB.1, DEFAULT_RGB.2)
    }
}

/// Parses a `#RRGGBB` hex color string, falling back safely to `#7aa2f7` on invalid input.
pub fn parse_hex_color(raw: &str) -> RgbColor {
    let Some(hex) = raw.trim().strip_prefix('#') else {
        return RgbColor::default();
    };
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return RgbColor::default();
    }

    let Ok(r) = u8::from_str_radix(&hex[0..2], 16) else {
        return RgbColor::default();
    };
    let Ok(g) = u8::from_str_radix(&hex[2..4], 16) else {
        return RgbColor::default();
    };
    let Ok(b) = u8::from_str_radix(&hex[4..6], 16) else {
        return RgbColor::default();
    };

    RgbColor::from_u8(r, g, b)
}

/// Pure animation and rendering state for [`EqualizerWidget`].
#[derive(Debug, Clone)]
pub struct EqualizerState {
    bar_count: usize,
    displayed_bars: Vec<f32>,
    target_bars: Vec<f32>,
    paused_or_silent: bool,
    active: bool,
    dirty: bool,
    color: RgbColor,
}

impl EqualizerState {
    /// Creates a new equalizer state initialized to silent resting bars.
    pub fn new(bar_count: usize, accent_color: &str) -> Self {
        let count = bar_count.max(1);
        Self {
            bar_count: count,
            displayed_bars: vec![0.0; count],
            target_bars: vec![0.0; count],
            paused_or_silent: true,
            active: false,
            dirty: false,
            color: parse_hex_color(accent_color),
        }
    }

    /// Updates the accent color from a `#RRGGBB` string.
    pub fn set_accent_color(&mut self, accent_color: &str) {
        self.color = parse_hex_color(accent_color);
        if self.active {
            self.dirty = true;
        }
    }

    /// Enables or disables frame redrawing based on overlay visibility.
    pub fn set_active(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            self.dirty = true;
        } else {
            self.displayed_bars.fill(0.0);
            self.target_bars.fill(0.0);
            self.dirty = false;
        }
    }

    /// Returns whether the widget is currently visible and animating.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Updates target bar amplitudes from the latest spectrum frame and playback pause state.
    pub fn update_frame(&mut self, frame: &SpectrumFrame, is_paused: bool) {
        self.paused_or_silent = is_paused || !frame.is_audible;
        if self.paused_or_silent || frame.bars.is_empty() {
            self.target_bars.fill(0.0);
            return;
        }

        let src_len = frame.bars.len();
        if src_len == self.bar_count {
            for (dst, &src) in self.target_bars.iter_mut().zip(&frame.bars) {
                *dst = sanitize_level(src);
            }
        } else {
            for (i, dst) in self.target_bars.iter_mut().enumerate() {
                let src_idx = (i * src_len) / self.bar_count;
                *dst = sanitize_level(frame.bars[src_idx]);
            }
        }
    }

    /// Advances bar interpolation by one ~16ms step when active.
    ///
    /// Returns `true` only when `active == true` and a redraw is needed.
    pub fn tick(&mut self) -> bool {
        if !self.active {
            return false;
        }

        let mut changed = self.dirty;
        self.dirty = false;

        for (current, &target) in self.displayed_bars.iter_mut().zip(&self.target_bars) {
            let diff = target - *current;
            if diff.abs() < SETTLE_EPS {
                if *current != target {
                    *current = target;
                    changed = true;
                }
                continue;
            }

            let alpha = if diff > 0.0 {
                ATTACK_ALPHA
            } else {
                DECAY_ALPHA
            };
            *current = (*current + alpha * diff).clamp(0.0, 1.0);
            changed = true;
        }

        changed
    }

    /// Returns the currently displayed normalized bar heights in `[0.0, 1.0]`.
    pub fn displayed_bars(&self) -> &[f32] {
        &self.displayed_bars
    }

    /// Returns the current target normalized bar heights in `[0.0, 1.0]`.
    pub fn target_bars(&self) -> &[f32] {
        &self.target_bars
    }

    /// Returns the parsed accent color.
    pub fn color(&self) -> RgbColor {
        self.color
    }
}

/// Custom GTK4 `DrawingArea` widget rendering centered rounded capsule equalizer bars.
#[derive(Clone)]
pub struct EqualizerWidget {
    area: gtk4::DrawingArea,
    state: Rc<RefCell<EqualizerState>>,
    timer_running: Rc<std::cell::Cell<bool>>,
}

impl EqualizerWidget {
    /// Creates a new [`EqualizerWidget`] with `bar_count` bars and an on-demand ~60 FPS GLib timer.
    pub fn new(bar_count: usize, accent_color: &str) -> Self {
        let count = bar_count.max(1);
        let state = Rc::new(RefCell::new(EqualizerState::new(count, accent_color)));

        let area = gtk4::DrawingArea::new();
        area.set_content_width(desired_width(count));
        area.set_content_height(WIDGET_HEIGHT_PX);
        area.set_valign(gtk4::Align::Center);
        area.add_css_class("wavepill-equalizer");

        let draw_state = Rc::clone(&state);
        area.set_draw_func(move |_area, cr, width, height| {
            let st = draw_state.borrow();
            draw_bars(cr, f64::from(width), f64::from(height), &st);
        });

        Self {
            area,
            state,
            timer_running: Rc::new(std::cell::Cell::new(false)),
        }
    }

    fn ensure_timer_started(&self) {
        if self.timer_running.replace(true) {
            return;
        }

        let area_weak = self.area.downgrade();
        let timer_state = Rc::downgrade(&self.state);
        let timer_running = Rc::clone(&self.timer_running);
        glib::timeout_add_local(Duration::from_millis(FRAME_INTERVAL_MS), move || {
            let (Some(area), Some(state_rc)) = (area_weak.upgrade(), timer_state.upgrade()) else {
                timer_running.set(false);
                return glib::ControlFlow::Break;
            };

            let mut st = state_rc.borrow_mut();
            if !st.is_active() {
                timer_running.set(false);
                return glib::ControlFlow::Break;
            }

            if st.tick() {
                area.queue_draw();
            }
            glib::ControlFlow::Continue
        });
    }

    /// Returns the underlying `gtk4::DrawingArea` widget.
    pub fn widget(&self) -> &gtk4::DrawingArea {
        &self.area
    }

    /// Enables or disables animation redraws according to capsule visibility.
    pub fn set_active(&self, active: bool) {
        let mut state = self.state.borrow_mut();
        let was_active = state.is_active();
        state.set_active(active);
        drop(state);

        if active {
            self.ensure_timer_started();
            if !was_active {
                self.area.queue_draw();
            }
        }
    }

    /// Feeds the latest audio spectrum frame and pause flag to the equalizer.
    pub fn update_frame(&self, frame: &SpectrumFrame, is_paused: bool) {
        self.state.borrow_mut().update_frame(frame, is_paused);
    }

    /// Updates the equalizer accent color.
    pub fn set_accent_color(&self, accent_color: &str) {
        self.state.borrow_mut().set_accent_color(accent_color);
        if self.state.borrow().is_active() {
            self.area.queue_draw();
        }
    }
}

fn desired_width(bar_count: usize) -> i32 {
    ((bar_count as i32) * 5 + 4).clamp(56, 96)
}

fn sanitize_level(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn draw_bars(cr: &cairo::Context, width: f64, height: f64, state: &EqualizerState) {
    let count = state.displayed_bars.len();
    if width <= 0.0 || height <= 0.0 || count == 0 {
        return;
    }

    let max_bar_h = (height - 2.0 * VERTICAL_PADDING_PX).max(2.0);
    let slot_w = width / count as f64;
    let bar_w = (slot_w * 0.56).clamp(1.6, 5.0);
    let min_h = bar_w.min(max_bar_h);
    let radius = bar_w * 0.5;
    let top_color = state.color.lightened(0.26);
    let base_color = state.color;

    for (idx, &raw_level) in state.displayed_bars.iter().enumerate() {
        let level = f64::from(sanitize_level(raw_level));
        let bar_h = min_h + (max_bar_h - min_h) * level;
        let x = idx as f64 * slot_w + (slot_w - bar_w) * 0.5;
        let y = (height - bar_h) * 0.5;
        let cx = x + radius;

        cr.new_path();
        cr.arc(cx, y + radius, radius, PI, 2.0 * PI);
        cr.arc(cx, y + bar_h - radius, radius, 0.0, PI);
        cr.close_path();

        let alpha = 0.42 + 0.58 * level;
        let grad = cairo::LinearGradient::new(0.0, y, 0.0, y + bar_h);
        grad.add_color_stop_rgba(0.0, top_color.r, top_color.g, top_color.b, alpha);
        grad.add_color_stop_rgba(1.0, base_color.r, base_color.g, base_color.b, alpha * 0.82);

        if cr.set_source(&grad).is_err() {
            cr.set_source_rgba(base_color.r, base_color.g, base_color.b, alpha);
        }
        let _ = cr.fill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_rrggbb_hex_colors() {
        assert_eq!(
            parse_hex_color("#7aa2f7"),
            RgbColor::from_u8(0x7a, 0xa2, 0xf7)
        );
        assert_eq!(
            parse_hex_color("#FF8800"),
            RgbColor::from_u8(0xff, 0x88, 0x00)
        );
        assert_eq!(parse_hex_color("  #000000  "), RgbColor::from_u8(0, 0, 0));
        assert_eq!(parse_hex_color("#ffffff"), RgbColor::from_u8(255, 255, 255));
    }

    #[test]
    fn invalid_hex_colors_fall_back_to_default_accent() {
        let fallback = RgbColor::from_u8(0x7a, 0xa2, 0xf7);
        for invalid in [
            "",
            "7aa2f7",
            "#fff",
            "#12345",
            "#1234567",
            "#7aa2f7ff",
            "#gggggg",
            "#+12345",
            "rgb(1, 2, 3)",
        ] {
            assert_eq!(
                parse_hex_color(invalid),
                fallback,
                "expected fallback for input {invalid:?}"
            );
        }
    }

    #[test]
    fn inactive_state_never_requests_redraw() {
        let mut state = EqualizerState::new(8, "#7aa2f7");
        let audible = SpectrumFrame {
            bars: vec![0.8; 8],
            rms: 0.2,
            is_audible: true,
        };
        state.update_frame(&audible, false);

        assert!(!state.is_active());
        assert!(!state.tick());
        assert!(state.displayed_bars().iter().all(|&v| v == 0.0));
    }

    #[test]
    fn active_state_interpolates_up_and_decays_smoothly_on_pause_or_silence() {
        let mut state = EqualizerState::new(4, "#bb9af7");
        state.set_active(true);

        let frame = SpectrumFrame {
            bars: vec![0.9, 0.5, 0.2, 1.5],
            rms: 0.3,
            is_audible: true,
        };
        state.update_frame(&frame, false);
        assert_eq!(state.target_bars(), &[0.9, 0.5, 0.2, 1.0]);

        assert!(state.tick());
        assert!(state.displayed_bars()[0] > 0.3 && state.displayed_bars()[0] < 0.9);
        assert!(state.displayed_bars()[3] > 0.4 && state.displayed_bars()[3] <= 1.0);

        for _ in 0..30 {
            state.tick();
        }
        assert!((state.displayed_bars()[0] - 0.9).abs() < 0.01);

        // Pausing immediately drives targets to zero and interpolates smoothly down.
        state.update_frame(&frame, true);
        assert_eq!(state.target_bars(), &[0.0; 4]);
        assert!(state.tick());
        assert!(state.displayed_bars()[0] < 0.9 && state.displayed_bars()[0] > 0.5);

        for _ in 0..80 {
            state.tick();
        }
        assert_eq!(state.displayed_bars(), &[0.0; 4]);
        // Settled state requires no further redraws until new activity arrives.
        assert!(!state.tick());
    }

    #[test]
    fn handles_mismatched_bar_counts_and_non_finite_values() {
        let mut state = EqualizerState::new(6, "#7aa2f7");
        state.set_active(true);

        let frame = SpectrumFrame {
            bars: vec![f32::NAN, 0.6, f32::INFINITY],
            rms: 0.1,
            is_audible: true,
        };
        state.update_frame(&frame, false);
        assert_eq!(state.target_bars().len(), 6);
        assert!(
            state
                .target_bars()
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
        );

        // Deactivating resets bars cleanly.
        state.tick();
        state.set_active(false);
        assert_eq!(state.displayed_bars(), &[0.0; 6]);
        assert_eq!(state.target_bars(), &[0.0; 6]);
    }
}
