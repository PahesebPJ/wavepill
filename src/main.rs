use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tracing_subscriber::EnvFilter;
use wavepill::audio::AudioEngine;
use wavepill::config::Config;
use wavepill::ipc::{
    IpcCommand, IpcServer, format_waybar_json, parse_command, query_status, send_command,
    signal_waybar, spawn_detached_daemon,
};
use wavepill::media::MediaController;
use wavepill::ui;

fn main() -> ExitCode {
    if let Some(arg) = std::env::args().nth(1) {
        return run_cli(&arg);
    }

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

    let enabled_flag = Arc::new(AtomicBool::new(true));
    let (mut ipc_server, ipc_rx) = IpcServer::start(Arc::clone(&enabled_flag));
    signal_waybar();

    let audio = AudioEngine::start(config.bar_count);
    let media = Arc::new(MediaController::start(config.ignore_apps.clone()));

    let spectrum_rx = audio.subscribe();
    let media_rx = media.subscribe();
    let media_for_commands = Arc::clone(&media);

    let exit_code = ui::run(
        config,
        spectrum_rx,
        media_rx,
        ipc_rx,
        Box::new(move |cmd| {
            tracing::debug!(?cmd, "dispatching media command");
            media_for_commands.send_command(cmd);
        }),
    );

    media.stop();
    audio.stop();
    if let Some(server) = ipc_server.as_mut() {
        server.stop();
    }
    signal_waybar();

    if exit_code == gtk4::glib::ExitCode::SUCCESS {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run_cli(arg: &str) -> ExitCode {
    let Some(cmd) = parse_command(arg) else {
        eprintln!("Usage: wavepill [toggle|enable|disable|reset-pos|status]");
        return ExitCode::FAILURE;
    };

    match cmd {
        IpcCommand::Status => {
            println!("{}", format_waybar_json(query_status()));
            ExitCode::SUCCESS
        }
        IpcCommand::Toggle => match send_command(IpcCommand::Toggle) {
            Ok(_) => {
                signal_waybar();
                ExitCode::SUCCESS
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                if let Err(spawn_err) = spawn_detached_daemon() {
                    eprintln!("Failed to start wavepill daemon: {spawn_err}");
                    return ExitCode::FAILURE;
                }
                std::thread::sleep(Duration::from_millis(180));
                signal_waybar();
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("Failed to communicate with wavepill daemon: {err}");
                ExitCode::FAILURE
            }
        },
        IpcCommand::Enable | IpcCommand::Disable | IpcCommand::ResetPos => {
            match send_command(cmd) {
                Ok(_) => {
                    signal_waybar();
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("wavepill daemon is not running: {err}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
