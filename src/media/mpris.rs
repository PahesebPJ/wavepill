//! MPRIS2 D-Bus player discovery, signal monitoring, prioritization, and command dispatch.

use std::collections::{HashMap, HashSet};
use std::future::poll_fn;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use zbus::fdo::{NameOwnerChanged, PropertiesChanged};
use zbus::message::Type as MessageType;
use zbus::zvariant::{Dict, OwnedValue, Value};
use zbus::{Connection, MatchRule, Message, MessageStream};

use super::types::{ControlMode, MediaCommand, MediaSnapshot, PlaybackStatus};

pub const MPRIS_BUS_PREFIX: &str = "org.mpris.MediaPlayer2.";
pub const MPRIS_OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
pub const MPRIS_ROOT_INTERFACE: &str = "org.mpris.MediaPlayer2";
pub const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

const DBUS_SERVICE: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";

const FALLBACK_RESYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Metadata extracted from `org.mpris.MediaPlayer2.Player.Metadata`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MprisMetadata {
    pub title: String,
    pub artist: String,
    pub art_url: Option<String>,
}

/// Snapshot of a tracked MPRIS2 player instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MprisPlayer {
    pub bus_name: String,
    pub identity: String,
    pub desktop_entry: Option<String>,
    pub status: PlaybackStatus,
    pub metadata: MprisMetadata,
    pub can_go_next: bool,
    pub can_go_previous: bool,
}

/// Command targeted at a specific active MPRIS bus name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MprisDispatch {
    pub bus_name: String,
    pub command: MediaCommand,
}

#[derive(Debug, Clone)]
struct TrackedPlayer {
    unique_owner: Option<String>,
    player: MprisPlayer,
}

#[derive(Debug, Default)]
struct MprisTracker {
    players: Vec<TrackedPlayer>,
    ignored_buses: HashMap<String, Option<String>>,
}

impl MprisTracker {
    fn active_player(&self) -> Option<MprisPlayer> {
        let players: Vec<MprisPlayer> = self
            .players
            .iter()
            .map(|entry| entry.player.clone())
            .collect();
        prioritize_players(&players).cloned()
    }

    fn remove_bus(&mut self, bus_name: &str) {
        self.players
            .retain(|entry| entry.player.bus_name != bus_name);
        self.ignored_buses.remove(bus_name);
    }

    fn upsert_player(&mut self, unique_owner: Option<String>, player: MprisPlayer) {
        self.ignored_buses.remove(&player.bus_name);

        if let Some(pos) = self
            .players
            .iter()
            .position(|entry| entry.player.bus_name == player.bus_name)
        {
            let status_changed = self.players[pos].player.status != player.status;
            self.players[pos].unique_owner = unique_owner;
            self.players[pos].player = player;
            if status_changed {
                let updated = self.players.remove(pos);
                self.players.push(updated);
            }
        } else {
            self.players.push(TrackedPlayer {
                unique_owner,
                player,
            });
        }
    }

    fn mark_ignored(&mut self, bus_name: String, unique_owner: Option<String>) {
        self.players
            .retain(|entry| entry.player.bus_name != bus_name);
        self.ignored_buses.insert(bus_name, unique_owner);
    }

    fn owns_ignored_sender(&self, sender: &str) -> bool {
        self.ignored_buses
            .values()
            .any(|owner| owner.as_deref() == Some(sender))
    }

    fn bus_names_for_sender(&self, sender: &str) -> Vec<String> {
        self.players
            .iter()
            .filter(|entry| entry.unique_owner.as_deref() == Some(sender))
            .map(|entry| entry.player.bus_name.clone())
            .collect()
    }
}

/// Returns `true` if `bus_name` follows the MPRIS2 well-known prefix.
pub fn is_mpris_bus_name(bus_name: &str) -> bool {
    bus_name.starts_with(MPRIS_BUS_PREFIX)
}

/// Parses an MPRIS2 `PlaybackStatus` string into [`PlaybackStatus`].
pub fn parse_playback_status(raw: &str) -> PlaybackStatus {
    match raw.trim() {
        "Playing" => PlaybackStatus::Playing,
        "Paused" => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    }
}

/// Resolves a human-friendly application name for an MPRIS player.
pub fn extract_player_app_name(
    bus_name: &str,
    identity: Option<&str>,
    desktop_entry: Option<&str>,
) -> String {
    if let Some(id) = identity.map(str::trim).filter(|s| !s.is_empty()) {
        return id.to_owned();
    }
    if let Some(entry) = desktop_entry.map(str::trim).filter(|s| !s.is_empty()) {
        return entry.to_owned();
    }
    let suffix = bus_name.strip_prefix(MPRIS_BUS_PREFIX).unwrap_or(bus_name);
    suffix.split('.').next().unwrap_or(suffix).to_owned()
}

/// Returns `true` when an MPRIS player matches any entry in `ignore_apps` (case-insensitive).
pub fn is_mpris_app_ignored(
    bus_name: &str,
    identity: Option<&str>,
    desktop_entry: Option<&str>,
    ignore_apps: &[String],
) -> bool {
    let suffix = bus_name.strip_prefix(MPRIS_BUS_PREFIX).unwrap_or(bus_name);
    let base_name = suffix.split('.').next().unwrap_or(suffix);

    let candidates = [
        Some(bus_name),
        Some(suffix),
        Some(base_name),
        identity.map(str::trim).filter(|s| !s.is_empty()),
        desktop_entry.map(str::trim).filter(|s| !s.is_empty()),
    ];

    ignore_apps.iter().any(|ignored| {
        let needle = ignored.trim();
        !needle.is_empty()
            && candidates
                .iter()
                .flatten()
                .any(|candidate| candidate.eq_ignore_ascii_case(needle))
    })
}

/// Deterministically selects the active MPRIS player:
/// prefers `Playing`, then the last `Paused`, otherwise `Stopped` (or `None`).
pub fn prioritize_players(players: &[MprisPlayer]) -> Option<&MprisPlayer> {
    if let Some(playing) = players
        .iter()
        .rfind(|player| player.status == PlaybackStatus::Playing)
    {
        return Some(playing);
    }

    if let Some(paused) = players
        .iter()
        .rfind(|player| player.status == PlaybackStatus::Paused)
    {
        return Some(paused);
    }

    players
        .iter()
        .rfind(|player| player.status == PlaybackStatus::Stopped)
}

/// Converts an [`MprisPlayer`] into a transport-agnostic [`MediaSnapshot`].
pub fn player_to_snapshot(player: &MprisPlayer) -> MediaSnapshot {
    MediaSnapshot {
        title: player.metadata.title.clone(),
        artist: player.metadata.artist.clone(),
        app_name: extract_player_app_name(
            &player.bus_name,
            Some(&player.identity),
            player.desktop_entry.as_deref(),
        ),
        art_url: player.metadata.art_url.clone(),
        status: player.status,
        control_mode: ControlMode::Mpris,
        can_go_next: player.can_go_next,
        can_go_previous: player.can_go_previous,
        is_muted: false,
    }
}

/// Extracts `xesam:title`, `xesam:artist`, and `mpris:artUrl` from an MPRIS `Metadata` value.
pub fn parse_metadata_value(value: &Value<'_>) -> MprisMetadata {
    match unwrap_value(value) {
        Value::Dict(dict) => parse_metadata_dict(dict),
        _ => MprisMetadata::default(),
    }
}

/// Extracts `xesam:title`, `xesam:artist`, and `mpris:artUrl` from a `zvariant::Dict`.
pub fn parse_metadata_dict(dict: &Dict<'_, '_>) -> MprisMetadata {
    let mut metadata = MprisMetadata::default();

    for (raw_key, raw_val) in dict.iter() {
        let Some(key) = value_as_str(raw_key) else {
            continue;
        };
        match key {
            "xesam:title" => {
                if let Some(title) = value_as_str(raw_val) {
                    metadata.title = title.trim().to_owned();
                }
            }
            "xesam:artist" => {
                metadata.artist = extract_artist(raw_val);
            }
            "mpris:artUrl" => {
                metadata.art_url = value_as_str(raw_val)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned);
            }
            _ => {}
        }
    }

    metadata
}

fn unwrap_value<'a>(mut value: &'a Value<'a>) -> &'a Value<'a> {
    while let Value::Value(inner) = value {
        value = inner;
    }
    value
}

fn value_as_str<'a>(value: &'a Value<'a>) -> Option<&'a str> {
    match unwrap_value(value) {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

fn value_as_bool(value: &Value<'_>) -> Option<bool> {
    match unwrap_value(value) {
        Value::Bool(flag) => Some(*flag),
        _ => None,
    }
}

fn extract_artist(value: &Value<'_>) -> String {
    match unwrap_value(value) {
        Value::Array(arr) => arr
            .inner()
            .iter()
            .filter_map(value_as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Value::Str(s) => s.as_str().trim().to_owned(),
        _ => String::new(),
    }
}

/// Runs the async MPRIS monitor on the provided Tokio runtime until `shutdown_rx` becomes `true`.
pub(crate) async fn run_mpris_monitor(
    ignore_apps: Vec<String>,
    active_tx: watch::Sender<Option<MprisPlayer>>,
    mut cmd_rx: mpsc::UnboundedReceiver<MprisDispatch>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    if *shutdown_rx.borrow() {
        return;
    }

    let conn = match Connection::session().await {
        Ok(conn) => conn,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "Failed to connect to D-Bus session bus; MPRIS monitoring disabled"
            );
            publish_active(&active_tx, None);
            while !*shutdown_rx.borrow() {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    cmd = cmd_rx.recv() => {
                        if cmd.is_none() {
                            break;
                        }
                    }
                }
            }
            return;
        }
    };

    let (mut owner_stream, mut props_stream) = subscribe_signal_streams(&conn).await;
    let mut fallback_polling = owner_stream.is_none() || props_stream.is_none();

    let mut tracker = MprisTracker::default();
    sync_all_players(&conn, &ignore_apps, &mut tracker).await;
    publish_active(&active_tx, tracker.active_player());

    let mut resync_timer = tokio::time::interval(FALLBACK_RESYNC_INTERVAL);
    resync_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
            maybe_cmd = cmd_rx.recv() => {
                let Some(dispatch) = maybe_cmd else {
                    break;
                };
                if let Err(err) = execute_mpris_command(&conn, &dispatch).await {
                    tracing::warn!(
                        bus_name = %dispatch.bus_name,
                        command = ?dispatch.command,
                        error = %err,
                        "Failed to execute MPRIS D-Bus command"
                    );
                }
            }
            maybe_owner_msg = next_optional_message(owner_stream.as_mut()), if owner_stream.is_some() => {
                match maybe_owner_msg {
                    Some(Ok(msg)) => {
                        handle_owner_changed(&conn, &msg, &ignore_apps, &mut tracker).await;
                        publish_active(&active_tx, tracker.active_player());
                    }
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "MPRIS NameOwnerChanged stream error; enabling fallback resync");
                        owner_stream = None;
                        fallback_polling = true;
                    }
                    None => {
                        tracing::warn!("MPRIS NameOwnerChanged stream ended; enabling fallback resync");
                        owner_stream = None;
                        fallback_polling = true;
                    }
                }
            }
            maybe_props_msg = next_optional_message(props_stream.as_mut()), if props_stream.is_some() => {
                match maybe_props_msg {
                    Some(Ok(msg)) => {
                        handle_properties_changed(&conn, &msg, &ignore_apps, &mut tracker).await;
                        publish_active(&active_tx, tracker.active_player());
                    }
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "MPRIS PropertiesChanged stream error; enabling fallback resync");
                        props_stream = None;
                        fallback_polling = true;
                    }
                    None => {
                        tracing::warn!("MPRIS PropertiesChanged stream ended; enabling fallback resync");
                        props_stream = None;
                        fallback_polling = true;
                    }
                }
            }
            _ = resync_timer.tick(), if fallback_polling => {
                sync_all_players(&conn, &ignore_apps, &mut tracker).await;
                publish_active(&active_tx, tracker.active_player());
                if owner_stream.is_none() || props_stream.is_none() {
                    let (new_owner, new_props) = subscribe_signal_streams(&conn).await;
                    if owner_stream.is_none() {
                        owner_stream = new_owner;
                    }
                    if props_stream.is_none() {
                        props_stream = new_props;
                    }
                    fallback_polling = owner_stream.is_none() || props_stream.is_none();
                }
            }
        }
    }
}

fn publish_active(tx: &watch::Sender<Option<MprisPlayer>>, next: Option<MprisPlayer>) {
    tx.send_if_modified(|current| {
        if *current != next {
            *current = next;
            true
        } else {
            false
        }
    });
}

async fn subscribe_signal_streams(
    conn: &Connection,
) -> (Option<MessageStream>, Option<MessageStream>) {
    let owner_stream = match build_owner_match_rule() {
        Ok(rule) => match MessageStream::for_match_rule(rule, conn, Some(64)).await {
            Ok(stream) => Some(stream),
            Err(err) => {
                tracing::warn!(error = %err, "Failed to subscribe to NameOwnerChanged signal");
                None
            }
        },
        Err(err) => {
            tracing::warn!(error = %err, "Invalid NameOwnerChanged match rule");
            None
        }
    };

    let props_stream = match build_properties_match_rule() {
        Ok(rule) => match MessageStream::for_match_rule(rule, conn, Some(128)).await {
            Ok(stream) => Some(stream),
            Err(err) => {
                tracing::warn!(error = %err, "Failed to subscribe to MPRIS PropertiesChanged signal");
                None
            }
        },
        Err(err) => {
            tracing::warn!(error = %err, "Invalid PropertiesChanged match rule");
            None
        }
    };

    (owner_stream, props_stream)
}

fn build_owner_match_rule() -> zbus::Result<MatchRule<'static>> {
    MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(DBUS_SERVICE)?
        .interface(DBUS_INTERFACE)?
        .member("NameOwnerChanged")?
        .arg0ns("org.mpris.MediaPlayer2")
        .map(|builder| builder.build())
}

fn build_properties_match_rule() -> zbus::Result<MatchRule<'static>> {
    MatchRule::builder()
        .msg_type(MessageType::Signal)
        .path(MPRIS_OBJECT_PATH)?
        .interface(PROPERTIES_INTERFACE)?
        .member("PropertiesChanged")
        .map(|builder| builder.build())
}

async fn next_optional_message(
    stream: Option<&mut MessageStream>,
) -> Option<zbus::Result<Message>> {
    use zbus::export::futures_core::stream::Stream;

    let stream = stream?;
    poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

async fn execute_mpris_command(conn: &Connection, dispatch: &MprisDispatch) -> zbus::Result<()> {
    let method = match dispatch.command {
        MediaCommand::PlayPause => "PlayPause",
        MediaCommand::Next => "Next",
        MediaCommand::Previous => "Previous",
        MediaCommand::ToggleMute => return Ok(()),
    };

    conn.call_method(
        Some(dispatch.bus_name.as_str()),
        MPRIS_OBJECT_PATH,
        Some(MPRIS_PLAYER_INTERFACE),
        method,
        &(),
    )
    .await?;

    Ok(())
}

async fn sync_all_players(conn: &Connection, ignore_apps: &[String], tracker: &mut MprisTracker) {
    let names = match list_mpris_names(conn).await {
        Ok(names) => names,
        Err(err) => {
            tracing::warn!(error = %err, "Failed to list D-Bus names for MPRIS discovery");
            return;
        }
    };

    let active_set: HashSet<String> = names.iter().cloned().collect();
    tracker
        .players
        .retain(|entry| active_set.contains(&entry.player.bus_name));
    tracker
        .ignored_buses
        .retain(|bus_name, _| active_set.contains(bus_name));

    for bus_name in names {
        refresh_player(conn, &bus_name, None, ignore_apps, tracker).await;
    }
}

async fn handle_owner_changed(
    conn: &Connection,
    msg: &Message,
    ignore_apps: &[String],
    tracker: &mut MprisTracker,
) {
    let Some(signal) = NameOwnerChanged::from_message(msg.clone()) else {
        return;
    };
    let Ok(args) = signal.args() else {
        return;
    };

    let bus_name = args.name().as_str();
    if !is_mpris_bus_name(bus_name) {
        return;
    }

    match args.new_owner().as_ref() {
        Some(new_owner) => {
            refresh_player(
                conn,
                bus_name,
                Some(new_owner.as_str().to_owned()),
                ignore_apps,
                tracker,
            )
            .await;
        }
        None => {
            tracker.remove_bus(bus_name);
        }
    }
}

async fn handle_properties_changed(
    conn: &Connection,
    msg: &Message,
    ignore_apps: &[String],
    tracker: &mut MprisTracker,
) {
    let Some(signal) = PropertiesChanged::from_message(msg.clone()) else {
        return;
    };
    let Ok(args) = signal.args() else {
        return;
    };

    let iface = args.interface_name().as_str();
    if iface != MPRIS_PLAYER_INTERFACE && iface != MPRIS_ROOT_INTERFACE {
        return;
    }

    let Some(sender) = msg.header().sender().map(|s| s.as_str().to_owned()) else {
        return;
    };

    let target_buses = tracker.bus_names_for_sender(&sender);
    if target_buses.is_empty() {
        if !tracker.owns_ignored_sender(&sender) {
            sync_all_players(conn, ignore_apps, tracker).await;
        }
        return;
    }

    let changed = args.changed_properties();
    let invalidated = args.invalidated_properties();
    let needs_requery = invalidated.iter().any(|prop| {
        matches!(
            *prop,
            "PlaybackStatus"
                | "Metadata"
                | "CanGoNext"
                | "CanGoPrevious"
                | "Identity"
                | "DesktopEntry"
        )
    });

    for bus_name in target_buses {
        if needs_requery {
            refresh_player(conn, &bus_name, Some(sender.clone()), ignore_apps, tracker).await;
            continue;
        }

        let Some(pos) = tracker
            .players
            .iter()
            .position(|entry| entry.player.bus_name == bus_name)
        else {
            continue;
        };

        let mut updated_player = tracker.players[pos].player.clone();
        if iface == MPRIS_ROOT_INTERFACE {
            apply_root_property_changes(&mut updated_player, changed);
        } else if iface == MPRIS_PLAYER_INTERFACE {
            apply_player_property_changes(&mut updated_player, changed);
        }

        if is_mpris_app_ignored(
            &updated_player.bus_name,
            Some(&updated_player.identity),
            updated_player.desktop_entry.as_deref(),
            ignore_apps,
        ) {
            tracker.mark_ignored(bus_name, Some(sender.clone()));
        } else {
            tracker.upsert_player(Some(sender.clone()), updated_player);
        }
    }
}

fn apply_root_property_changes(player: &mut MprisPlayer, changed: &HashMap<&str, Value<'_>>) {
    if let Some(identity) = changed.get("Identity").and_then(|v| value_as_str(v)) {
        player.identity = identity.trim().to_owned();
    }
    if let Some(entry) = changed.get("DesktopEntry").and_then(|v| value_as_str(v)) {
        let trimmed = entry.trim();
        player.desktop_entry = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        };
    }
}

fn apply_player_property_changes(player: &mut MprisPlayer, changed: &HashMap<&str, Value<'_>>) {
    if let Some(status_raw) = changed.get("PlaybackStatus").and_then(|v| value_as_str(v)) {
        player.status = parse_playback_status(status_raw);
    }
    if let Some(metadata_val) = changed.get("Metadata") {
        player.metadata = parse_metadata_value(metadata_val);
    }
    if let Some(can_next) = changed.get("CanGoNext").and_then(value_as_bool) {
        player.can_go_next = can_next;
    }
    if let Some(can_prev) = changed.get("CanGoPrevious").and_then(value_as_bool) {
        player.can_go_previous = can_prev;
    }
}

async fn refresh_player(
    conn: &Connection,
    bus_name: &str,
    known_owner: Option<String>,
    ignore_apps: &[String],
    tracker: &mut MprisTracker,
) {
    let unique_owner = match known_owner {
        Some(owner) => Some(owner),
        None => get_name_owner(conn, bus_name).await.ok(),
    };

    if is_mpris_app_ignored(bus_name, None, None, ignore_apps) {
        tracker.mark_ignored(bus_name.to_owned(), unique_owner);
        return;
    }

    match query_player_properties(conn, bus_name).await {
        Ok(player) => {
            if is_mpris_app_ignored(
                &player.bus_name,
                Some(&player.identity),
                player.desktop_entry.as_deref(),
                ignore_apps,
            ) {
                tracker.mark_ignored(bus_name.to_owned(), unique_owner);
            } else {
                tracker.upsert_player(unique_owner, player);
            }
        }
        Err(err) => {
            tracing::debug!(
                bus_name = %bus_name,
                error = %err,
                "Failed to query MPRIS properties for player"
            );
            tracker.remove_bus(bus_name);
        }
    }
}

async fn list_mpris_names(conn: &Connection) -> zbus::Result<Vec<String>> {
    let reply = conn
        .call_method(
            Some(DBUS_SERVICE),
            DBUS_PATH,
            Some(DBUS_INTERFACE),
            "ListNames",
            &(),
        )
        .await?;
    let names: Vec<String> = reply.body().deserialize()?;
    Ok(names
        .into_iter()
        .filter(|name| is_mpris_bus_name(name))
        .collect())
}

async fn get_name_owner(conn: &Connection, bus_name: &str) -> zbus::Result<String> {
    let reply = conn
        .call_method(
            Some(DBUS_SERVICE),
            DBUS_PATH,
            Some(DBUS_INTERFACE),
            "GetNameOwner",
            &(bus_name,),
        )
        .await?;
    reply.body().deserialize()
}

async fn query_player_properties(conn: &Connection, bus_name: &str) -> zbus::Result<MprisPlayer> {
    let player_props = get_all_properties(conn, bus_name, MPRIS_PLAYER_INTERFACE).await?;
    let root_props = get_all_properties(conn, bus_name, MPRIS_ROOT_INTERFACE)
        .await
        .unwrap_or_default();

    let identity = root_props
        .get("Identity")
        .and_then(|v| value_as_str(v))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| extract_player_app_name(bus_name, None, None));

    let desktop_entry = root_props
        .get("DesktopEntry")
        .and_then(|v| value_as_str(v))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);

    let status = player_props
        .get("PlaybackStatus")
        .and_then(|v| value_as_str(v))
        .map(parse_playback_status)
        .unwrap_or(PlaybackStatus::Stopped);

    let metadata = player_props
        .get("Metadata")
        .map(|v| parse_metadata_value(v))
        .unwrap_or_default();

    let can_go_next = player_props
        .get("CanGoNext")
        .and_then(|v| value_as_bool(v))
        .unwrap_or(false);

    let can_go_previous = player_props
        .get("CanGoPrevious")
        .and_then(|v| value_as_bool(v))
        .unwrap_or(false);

    Ok(MprisPlayer {
        bus_name: bus_name.to_owned(),
        identity,
        desktop_entry,
        status,
        metadata,
        can_go_next,
        can_go_previous,
    })
}

async fn get_all_properties(
    conn: &Connection,
    bus_name: &str,
    interface: &str,
) -> zbus::Result<HashMap<String, OwnedValue>> {
    let reply = conn
        .call_method(
            Some(bus_name),
            MPRIS_OBJECT_PATH,
            Some(PROPERTIES_INTERFACE),
            "GetAll",
            &(interface,),
        )
        .await?;
    reply.body().deserialize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Signature};

    fn make_player(bus_suffix: &str, identity: &str, status: PlaybackStatus) -> MprisPlayer {
        MprisPlayer {
            bus_name: format!("{MPRIS_BUS_PREFIX}{bus_suffix}"),
            identity: identity.to_owned(),
            desktop_entry: Some(
                bus_suffix
                    .split('.')
                    .next()
                    .unwrap_or(bus_suffix)
                    .to_owned(),
            ),
            status,
            metadata: MprisMetadata {
                title: format!("{identity} Track"),
                artist: format!("{identity} Artist"),
                art_url: Some("file:///tmp/cover.png".to_owned()),
            },
            can_go_next: true,
            can_go_previous: true,
        }
    }

    #[test]
    fn prioritize_prefers_playing_over_paused_and_stopped() {
        let players = vec![
            make_player("vlc", "VLC", PlaybackStatus::Paused),
            make_player("spotify", "Spotify", PlaybackStatus::Playing),
            make_player("mpv", "mpv", PlaybackStatus::Paused),
            make_player("rhythmbox", "Rhythmbox", PlaybackStatus::Stopped),
        ];

        let selected = prioritize_players(&players).expect("should select playing player");
        assert_eq!(selected.identity, "Spotify");
        assert_eq!(selected.status, PlaybackStatus::Playing);
    }

    #[test]
    fn prioritize_picks_last_paused_when_none_playing() {
        let players = vec![
            make_player("vlc", "VLC", PlaybackStatus::Paused),
            make_player("rhythmbox", "Rhythmbox", PlaybackStatus::Stopped),
            make_player("spotify", "Spotify", PlaybackStatus::Paused),
        ];

        let selected = prioritize_players(&players).expect("should select last paused player");
        assert_eq!(selected.identity, "Spotify");
        assert_eq!(selected.status, PlaybackStatus::Paused);
    }

    #[test]
    fn prioritize_falls_back_to_stopped_and_none_when_empty() {
        assert_eq!(prioritize_players(&[]), None);

        let players = vec![
            make_player("vlc", "VLC", PlaybackStatus::Stopped),
            make_player("mpv", "mpv", PlaybackStatus::Stopped),
        ];
        let selected = prioritize_players(&players).expect("should select stopped player");
        assert_eq!(selected.identity, "mpv");
        assert_eq!(selected.status, PlaybackStatus::Stopped);
    }

    #[test]
    fn tracker_moves_status_changed_player_to_back_for_last_paused_order() {
        let mut tracker = MprisTracker::default();
        tracker.upsert_player(
            Some(":1.1".to_owned()),
            make_player("spotify", "Spotify", PlaybackStatus::Playing),
        );
        tracker.upsert_player(
            Some(":1.2".to_owned()),
            make_player("vlc", "VLC", PlaybackStatus::Paused),
        );

        assert_eq!(tracker.active_player().unwrap().identity, "Spotify");

        // Pause Spotify -> Spotify became paused after VLC, so Spotify must be last Paused.
        let mut paused_spotify = make_player("spotify", "Spotify", PlaybackStatus::Paused);
        paused_spotify.metadata.title = "Paused Track".to_owned();
        tracker.upsert_player(Some(":1.1".to_owned()), paused_spotify);

        let active = tracker.active_player().unwrap();
        assert_eq!(active.identity, "Spotify");
        assert_eq!(active.status, PlaybackStatus::Paused);
    }

    #[test]
    fn ignore_apps_matches_case_insensitively_across_bus_identity_and_desktop() {
        let ignore = vec!["  Spotify ".to_owned(), "ZEN".to_owned(), "".to_owned()];

        assert!(is_mpris_app_ignored(
            "org.mpris.MediaPlayer2.spotify",
            Some("Spotify"),
            Some("spotify"),
            &ignore
        ));
        assert!(is_mpris_app_ignored(
            "org.mpris.MediaPlayer2.firefox.instance_1_65",
            Some("Mozilla zen"),
            Some("zen"),
            &ignore
        ));
        assert!(is_mpris_app_ignored(
            "org.mpris.MediaPlayer2.SPOTIFY.instance2",
            None,
            None,
            &ignore
        ));
        assert!(!is_mpris_app_ignored(
            "org.mpris.MediaPlayer2.mpv",
            Some("mpv"),
            Some("mpv"),
            &ignore
        ));
    }

    #[test]
    fn parse_metadata_extracts_title_artist_array_and_art_url() {
        let mut dict = Dict::new(&Signature::Str, &Signature::Variant);
        dict.add("xesam:title", Value::new("Night Drive")).unwrap();

        let mut artists = Array::new(&Signature::Str);
        artists.append(Value::from("Artist A")).unwrap();
        artists.append(Value::from("Artist B")).unwrap();
        dict.add("xesam:artist", Value::new(artists)).unwrap();

        dict.add("mpris:artUrl", Value::new("file:///tmp/art.jpg"))
            .unwrap();

        let parsed = parse_metadata_value(&Value::Dict(dict));
        assert_eq!(parsed.title, "Night Drive");
        assert_eq!(parsed.artist, "Artist A, Artist B");
        assert_eq!(parsed.art_url.as_deref(), Some("file:///tmp/art.jpg"));
    }

    #[test]
    fn parse_metadata_handles_single_string_artist_and_empty_art_url() {
        let mut map: HashMap<&str, Value<'_>> = HashMap::new();
        map.insert("xesam:title", Value::from("Solo"));
        map.insert("xesam:artist", Value::from("Single Artist"));
        map.insert("mpris:artUrl", Value::from("   "));

        let parsed = parse_metadata_value(&Value::from(map));
        assert_eq!(parsed.title, "Solo");
        assert_eq!(parsed.artist, "Single Artist");
        assert_eq!(parsed.art_url, None);
    }

    #[test]
    fn player_to_snapshot_maps_all_fields() {
        let player = make_player("vlc", "VLC media player", PlaybackStatus::Playing);
        let snapshot = player_to_snapshot(&player);

        assert_eq!(snapshot.title, "VLC media player Track");
        assert_eq!(snapshot.artist, "VLC media player Artist");
        assert_eq!(snapshot.app_name, "VLC media player");
        assert_eq!(snapshot.art_url.as_deref(), Some("file:///tmp/cover.png"));
        assert_eq!(snapshot.status, PlaybackStatus::Playing);
        assert_eq!(snapshot.control_mode, ControlMode::Mpris);
        assert!(snapshot.can_go_next);
        assert!(snapshot.can_go_previous);
        assert!(!snapshot.is_muted);
    }
}
