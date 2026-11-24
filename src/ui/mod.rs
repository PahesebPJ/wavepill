//! Native Wayland GTK4 + layer-shell Dynamic Island UI coordinator.

pub mod equalizer;
pub mod island;
pub mod window;

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::{gdk, glib, prelude::*};
use tokio::sync::watch;

use crate::audio::SpectrumFrame;
use crate::config::Config;
use crate::media::{MediaCommand, MediaSnapshot, PlaybackStatus};
use crate::state::{IslandEvent, IslandStateMachine};

use self::island::IslandWidget;
use self::window::WavepillWindow;

const APP_ID: &str = "dev.wavepill.overlay";
const STYLE_CSS: &str = include_str!("style.css");
const STATE_POLL_INTERVAL_MS: u64 = 33;

struct UiControllerState {
    machine: IslandStateMachine,
    last_frame: SpectrumFrame,
    last_snapshot: MediaSnapshot,
}

impl UiControllerState {
    fn new(
        hide_delay: Duration,
        initial_frame: SpectrumFrame,
        initial_snapshot: MediaSnapshot,
    ) -> Self {
        let mut machine = IslandStateMachine::new(hide_delay);
        let now = Instant::now();
        machine.handle(IslandEvent::AudioActivity(initial_frame.is_audible), now);
        machine.handle(IslandEvent::MediaUpdated(initial_snapshot.clone()), now);

        Self {
            machine,
            last_frame: initial_frame,
            last_snapshot: initial_snapshot,
        }
    }

    fn is_paused(&self) -> bool {
        self.last_snapshot.status == PlaybackStatus::Paused
    }
}

/// Starts the GTK4 + Wayland layer-shell UI event loop on the current thread.
pub fn run(
    config: Config,
    spectrum_rx: watch::Receiver<SpectrumFrame>,
    media_rx: watch::Receiver<MediaSnapshot>,
    on_command: Box<dyn Fn(MediaCommand) + 'static>,
) -> glib::ExitCode {
    let app = gtk4::Application::builder().application_id(APP_ID).build();
    let on_command: Rc<dyn Fn(MediaCommand)> = Rc::from(on_command);
    let initialized = Rc::new(Cell::new(false));

    app.connect_activate(move |app| {
        if initialized.replace(true) {
            return;
        }

        load_css();

        let hold_guard = app.hold();
        let mut spectrum_rx = spectrum_rx.clone();
        let mut media_rx = media_rx.clone();

        let initial_frame = spectrum_rx.borrow_and_update().clone();
        let initial_snapshot = media_rx.borrow_and_update().clone();

        let state = Rc::new(RefCell::new(UiControllerState::new(
            config.hide_delay(),
            initial_frame.clone(),
            initial_snapshot.clone(),
        )));

        let ui_handles: Rc<OnceCell<(WavepillWindow, IslandWidget)>> = Rc::new(OnceCell::new());

        let hover_state = Rc::clone(&state);
        // Use a Weak handle to avoid a reference cycle:
        // ui_handles -> IslandWidget -> EventControllerMotion -> on_hover -> ui_handles.
        let hover_handles = Rc::downgrade(&ui_handles);
        let on_hover: Rc<dyn Fn(bool)> = Rc::new(move |entered| {
            let (visible, expanded) = {
                let mut st = hover_state.borrow_mut();
                let event = if entered {
                    IslandEvent::PointerEntered
                } else {
                    IslandEvent::PointerLeft
                };
                st.machine.handle(event, Instant::now());
                (st.machine.is_visible(), st.machine.is_expanded())
            };

            if let Some(handles) = hover_handles.upgrade()
                && let Some((win, island)) = handles.get()
            {
                win.set_overlay_visible(visible);
                island.set_expanded(expanded);
            }
        });

        let island = IslandWidget::new(&config, on_hover, Rc::clone(&on_command));
        let window = WavepillWindow::new(app, &config, island.clone());

        let _ = ui_handles.set((window.clone(), island.clone()));

        {
            let st = state.borrow();
            island.update_media(&initial_snapshot);
            island.update_spectrum(&initial_frame, st.is_paused());
            window.set_overlay_visible(st.machine.is_visible());
            island.set_expanded(st.machine.is_expanded());
        }

        let destroyed = Rc::new(Cell::new(false));
        let destroyed_on_close = Rc::clone(&destroyed);
        window.widget().connect_destroy(move |_| {
            destroyed_on_close.set(true);
        });

        let timer_state = Rc::clone(&state);
        let timer_handles = ui_handles;
        glib::timeout_add_local(Duration::from_millis(STATE_POLL_INTERVAL_MS), move || {
            let _keep_alive = &hold_guard;
            if destroyed.get() {
                return glib::ControlFlow::Break;
            }
            let Some((timer_window, timer_island)) = timer_handles.get() else {
                return glib::ControlFlow::Break;
            };

            let media_changed = media_rx.has_changed().unwrap_or(false);
            let spectrum_changed = spectrum_rx.has_changed().unwrap_or(false);

            let new_snapshot = if media_changed {
                Some(media_rx.borrow_and_update().clone())
            } else {
                None
            };
            let new_frame = if spectrum_changed {
                Some(spectrum_rx.borrow_and_update().clone())
            } else {
                None
            };

            let (media_update, spectrum_update, visible, expanded) = {
                let mut st = timer_state.borrow_mut();
                let now = Instant::now();

                if let Some(snapshot) = new_snapshot.as_ref() {
                    st.last_snapshot = snapshot.clone();
                    st.machine
                        .handle(IslandEvent::MediaUpdated(snapshot.clone()), now);
                }

                if let Some(frame) = new_frame.as_ref() {
                    st.last_frame = frame.clone();
                    st.machine
                        .handle(IslandEvent::AudioActivity(frame.is_audible), now);
                }

                st.machine.handle(IslandEvent::Tick(now), now);

                let spectrum_update = if media_changed || spectrum_changed {
                    Some((st.last_frame.clone(), st.is_paused()))
                } else {
                    None
                };

                (
                    new_snapshot,
                    spectrum_update,
                    st.machine.is_visible(),
                    st.machine.is_expanded(),
                )
            };

            if let Some(snapshot) = media_update {
                timer_island.update_media(&snapshot);
            }
            if let Some((frame, is_paused)) = spectrum_update {
                timer_island.update_spectrum(&frame, is_paused);
            }

            timer_window.set_overlay_visible(visible);
            timer_island.set_expanded(expanded);

            glib::ControlFlow::Continue
        });
    });

    app.run_with_args::<&str>(&[])
}

fn load_css() {
    let Some(display) = gdk::Display::default() else {
        tracing::warn!("No default GDK display available; skipping CSS provider registration");
        return;
    };

    let provider = gtk4::CssProvider::new();
    provider.load_from_string(STYLE_CSS);
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
