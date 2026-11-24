use std::process::ExitCode;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;
use wavepill::audio::AudioEngine;
use wavepill::config::Config;
use wavepill::media::MediaController;
use wavepill::ui;

fn main() -> ExitCode {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("wavepill=info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let config = Config::load_default();
    tracing::info!(
        corner = ?config.corner,
        margin_x = config.margin_x,
        margin_y = config.margin_y,
        bar_count = config.bar_count,
        hide_delay_ms = config.hide_delay_ms,
        "starting wavepill"
    );

    let audio = AudioEngine::start(config.bar_count);
    let media = Arc::new(MediaController::start(config.ignore_apps.clone()));

    let spectrum_rx = audio.subscribe();
    let media_rx = media.subscribe();
    let media_for_commands = Arc::clone(&media);

    let exit_code = ui::run(
        config,
        spectrum_rx,
        media_rx,
        Box::new(move |cmd| {
            tracing::debug!(?cmd, "dispatching media command");
            media_for_commands.send_command(cmd);
        }),
    );

    media.stop();
    audio.stop();

    if exit_code == gtk4::glib::ExitCode::SUCCESS {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
