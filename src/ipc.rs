//! Local Unix-socket IPC server and CLI client for Waybar/user control.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Real-time signal number used to refresh the Waybar `custom/wavepill` module.
pub const WAYBAR_SIGNAL: u32 = 11;

const SOCKET_IO_TIMEOUT: Duration = Duration::from_millis(500);
const ACCEPT_POLL_SLEEP: Duration = Duration::from_millis(40);
const MAX_IPC_LINE_BYTES: u64 = 256;

/// Commands accepted over the local IPC socket or CLI subcommands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcCommand {
    Toggle,
    Enable,
    Disable,
    ResetPos,
    Status,
}

impl IpcCommand {
    /// Returns the wire-protocol string for this command.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Toggle => "toggle",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::ResetPos => "reset-pos",
            Self::Status => "status",
        }
    }
}

/// Parses a CLI argument or IPC wire line into an [`IpcCommand`].
pub fn parse_command(raw: &str) -> Option<IpcCommand> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "toggle" => Some(IpcCommand::Toggle),
        "enable" | "on" => Some(IpcCommand::Enable),
        "disable" | "off" => Some(IpcCommand::Disable),
        "reset-pos" | "reset-position" | "reset" => Some(IpcCommand::ResetPos),
        "status" => Some(IpcCommand::Status),
        _ => None,
    }
}

/// High-level daemon status reported to Waybar (`wavepill status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStatus {
    Enabled,
    Disabled,
    Stopped,
}

/// Events forwarded from the IPC server thread to the GTK4 UI loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcEvent {
    SetEnabled(bool),
    ResetPosition,
}

/// Formats a single-line JSON payload compatible with Waybar's `return-type: "json"`.
pub fn format_waybar_json(status: DaemonStatus) -> String {
    match status {
        DaemonStatus::Enabled => {
            r#"{"text":"󱑽","class":"enabled","tooltip":"Wavepill: activo\nClic izq: desactivar\nClic der: restablecer posición"}"#
                .to_owned()
        }
        DaemonStatus::Disabled => {
            r#"{"text":"󰎊","class":"disabled","tooltip":"Wavepill: desactivado\nClic izq: activar\nClic der: restablecer posición"}"#
                .to_owned()
        }
        DaemonStatus::Stopped => {
            r#"{"text":"󰎊","class":"stopped","tooltip":"Wavepill: detenido\nClic izq: iniciar"}"#
                .to_owned()
        }
    }
}

/// Returns the default Unix domain socket path (`$XDG_RUNTIME_DIR/wavepill.sock`,
/// or a per-user `0700` directory under `/tmp` when `XDG_RUNTIME_DIR` is unset).
pub fn socket_path() -> PathBuf {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR")
        && !runtime_dir.is_empty()
    {
        return PathBuf::from(runtime_dir).join("wavepill.sock");
    }
    let user_tag = std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "default".to_owned());
    let dir = std::env::temp_dir().join(format!("wavepill-{user_tag}"));
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir.join("wavepill.sock")
}

/// Sends a command to the default wavepill socket and returns the trimmed reply.
pub fn send_command(cmd: IpcCommand) -> std::io::Result<String> {
    send_command_to_path(&socket_path(), cmd)
}

/// Sends a command to a specific Unix socket path and returns the trimmed reply.
pub fn send_command_to_path(path: &Path, cmd: IpcCommand) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(SOCKET_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT))?;

    writeln!(stream, "{}", cmd.as_str())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response.trim().to_owned())
}

/// Queries the running daemon's status, returning [`DaemonStatus::Stopped`] when unreachable.
pub fn query_status() -> DaemonStatus {
    query_status_at(&socket_path())
}

/// Queries status from an explicit Unix socket path.
pub fn query_status_at(path: &Path) -> DaemonStatus {
    match send_command_to_path(path, IpcCommand::Status) {
        Ok(reply) if reply == "enabled" => DaemonStatus::Enabled,
        Ok(reply) if reply == "disabled" => DaemonStatus::Disabled,
        _ => DaemonStatus::Stopped,
    }
}

/// Signals Waybar to refresh its `custom/wavepill` module immediately.
pub fn signal_waybar() {
    let signal_arg = format!("-RTMIN+{WAYBAR_SIGNAL}");
    let _ = Command::new("pkill")
        .arg("-x")
        .arg(signal_arg)
        .arg("waybar")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Spawns a detached `wavepill` daemon instance using the current executable path.
pub fn spawn_detached_daemon() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    Command::new(exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

/// Background Unix-socket server accepting control commands for the running daemon.
pub struct IpcServer {
    socket_path: PathBuf,
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl IpcServer {
    /// Starts the IPC listener on the default socket path.
    pub fn start(enabled: Arc<AtomicBool>) -> (Option<Self>, Receiver<IpcEvent>) {
        Self::start_at(socket_path(), enabled)
    }

    /// Starts the IPC listener on an explicit socket path.
    pub fn start_at(path: PathBuf, enabled: Arc<AtomicBool>) -> (Option<Self>, Receiver<IpcEvent>) {
        let (event_tx, event_rx) = mpsc::channel();

        if std::fs::symlink_metadata(&path).is_ok() {
            if UnixStream::connect(&path).is_ok() {
                tracing::warn!(
                    path = %path.display(),
                    "Another wavepill IPC instance appears active; skipping socket bind"
                );
                return (None, event_rx);
            }
            let _ = std::fs::remove_file(&path);
        }

        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "Failed to bind wavepill IPC socket"
                );
                return (None, event_rx);
            }
        };

        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));

        if let Err(err) = listener.set_nonblocking(true) {
            tracing::warn!(error = %err, "Failed to set IPC socket nonblocking");
            let _ = std::fs::remove_file(&path);
            return (None, event_rx);
        }

        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop_flag);
        let thread_path = path.clone();

        let handle = thread::Builder::new()
            .name("wavepill-ipc".to_owned())
            .spawn(move || {
                run_ipc_loop(listener, &thread_path, &thread_stop, &enabled, &event_tx);
            })
            .ok();

        if handle.is_none() {
            let _ = std::fs::remove_file(&path);
            return (None, event_rx);
        }

        (
            Some(Self {
                socket_path: path,
                stop_flag,
                handle,
            }),
            event_rx,
        )
    }

    /// Signals the IPC listener thread to stop and cleans up the socket file (idempotent).
    pub fn stop(&mut self) {
        if self.stop_flag.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_ipc_loop(
    listener: UnixListener,
    socket_path: &Path,
    stop_flag: &AtomicBool,
    enabled: &AtomicBool,
    event_tx: &Sender<IpcEvent>,
) {
    tracing::debug!(path = %socket_path.display(), "wavepill IPC server listening");

    while !stop_flag.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                handle_client(stream, enabled, event_tx);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_SLEEP);
            }
            Err(err) => {
                tracing::warn!(error = %err, "IPC accept error");
                thread::sleep(ACCEPT_POLL_SLEEP);
            }
        }
    }
}

fn handle_client(mut stream: UnixStream, enabled: &AtomicBool, event_tx: &Sender<IpcEvent>) {
    let _ = stream.set_read_timeout(Some(SOCKET_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT));

    let mut line = String::new();
    {
        let mut reader = BufReader::new(&stream).take(MAX_IPC_LINE_BYTES);
        if reader.read_line(&mut line).is_err() {
            return;
        }
    }

    let reply = match parse_command(&line) {
        Some(IpcCommand::Status) => {
            if enabled.load(Ordering::Acquire) {
                "enabled"
            } else {
                "disabled"
            }
        }
        Some(IpcCommand::Toggle) => {
            let prev = enabled.fetch_xor(true, Ordering::AcqRel);
            let next = !prev;
            let _ = event_tx.send(IpcEvent::SetEnabled(next));
            if next { "enabled" } else { "disabled" }
        }
        Some(IpcCommand::Enable) => {
            enabled.store(true, Ordering::Release);
            let _ = event_tx.send(IpcEvent::SetEnabled(true));
            "enabled"
        }
        Some(IpcCommand::Disable) => {
            enabled.store(false, Ordering::Release);
            let _ = event_tx.send(IpcEvent::SetEnabled(false));
            "disabled"
        }
        Some(IpcCommand::ResetPos) => {
            let _ = event_tx.send(IpcEvent::ResetPosition);
            "ok"
        }
        None => "error:unknown-command",
    };

    let _ = writeln!(stream, "{reply}");
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static SOCK_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_socket_path(label: &str) -> PathBuf {
        let id = SOCK_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wavepill_ipc_test_{}_{}_{}.sock",
            std::process::id(),
            id,
            label
        ))
    }

    #[test]
    fn parses_supported_commands_case_insensitively() {
        assert_eq!(parse_command("toggle\n"), Some(IpcCommand::Toggle));
        assert_eq!(parse_command("ENABLE"), Some(IpcCommand::Enable));
        assert_eq!(parse_command("off"), Some(IpcCommand::Disable));
        assert_eq!(parse_command("reset-pos"), Some(IpcCommand::ResetPos));
        assert_eq!(parse_command("status"), Some(IpcCommand::Status));
        assert_eq!(parse_command("unknown"), None);
    }

    #[test]
    fn formats_valid_waybar_json_for_all_statuses() {
        let enabled = format_waybar_json(DaemonStatus::Enabled);
        assert!(enabled.contains("\"class\":\"enabled\""));
        assert!(enabled.contains("\"text\":\"󱑽\""));

        let disabled = format_waybar_json(DaemonStatus::Disabled);
        assert!(disabled.contains("\"class\":\"disabled\""));

        let stopped = format_waybar_json(DaemonStatus::Stopped);
        assert!(stopped.contains("\"class\":\"stopped\""));
    }

    #[test]
    fn server_handles_status_toggle_enable_disable_and_reset_roundtrip() {
        let path = temp_socket_path("roundtrip");
        let enabled = Arc::new(AtomicBool::new(true));
        let (server, rx) = IpcServer::start_at(path.clone(), Arc::clone(&enabled));
        let mut server = server.expect("IPC server should bind");

        assert_eq!(query_status_at(&path), DaemonStatus::Enabled);

        assert_eq!(
            send_command_to_path(&path, IpcCommand::Toggle).unwrap(),
            "disabled"
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            IpcEvent::SetEnabled(false)
        );
        assert_eq!(query_status_at(&path), DaemonStatus::Disabled);

        assert_eq!(
            send_command_to_path(&path, IpcCommand::Enable).unwrap(),
            "enabled"
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            IpcEvent::SetEnabled(true)
        );

        assert_eq!(
            send_command_to_path(&path, IpcCommand::Disable).unwrap(),
            "disabled"
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            IpcEvent::SetEnabled(false)
        );

        assert_eq!(
            send_command_to_path(&path, IpcCommand::ResetPos).unwrap(),
            "ok"
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            IpcEvent::ResetPosition
        );

        server.stop();
        assert!(!path.exists());
        assert_eq!(query_status_at(&path), DaemonStatus::Stopped);
    }
}
