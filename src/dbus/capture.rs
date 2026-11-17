//! The recorder behind the halo's Record: `inc.kora.CaptureEngine1`, served by
//! kora-capture-engine and started on demand by the bus, through its client crate.

use std::collections::HashMap;

use anyhow::{Context, Result};
use calloop::{InsertError, stream::StreamSource};
use futures_util::{StreamExt, stream::select};
use kora_capture_client::engine::{BUS_NAME, CaptureEngine1Proxy, Codec, RecordOptions, Source};
use tracing::debug;
use zbus::fdo::DBusProxy;
use zbus::names::BusName;
use zbus::proxy::CacheProperties;

use super::DBusState;
use crate::state::State;

/// The recorder's answer, back on the compositor thread.
pub type Reply = Result<String, String>;

/// The recorder's answer to a stop: the finished file's path, or why not.
pub type StopReply = Result<String, StopFailure>;

/// Why a stop did not hand back a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopFailure {
    /// The engine has no recording by that id: it ended without a word.
    NoSuchRecording,
    Failed(String),
}

/// `StopRecording` takes only an id, so the engine's InvalidArgs means it has
/// no such recording.
const NO_SUCH_RECORDING: &str = "org.freedesktop.DBus.Error.InvalidArgs";

impl From<zbus::Error> for StopFailure {
    fn from(err: zbus::Error) -> Self {
        let no_such = match &err {
            zbus::Error::MethodError(name, ..) => name.as_str() == NO_SUCH_RECORDING,
            zbus::Error::FDO(fdo) => matches!(**fdo, zbus::fdo::Error::InvalidArgs(_)),
            _ => false,
        };
        if no_such {
            Self::NoSuchRecording
        } else {
            Self::Failed(err.to_string())
        }
    }
}

impl DBusState {
    /// Record the toplevel `identifier` into `path`, with sound from the engine's audio
    /// id `audio`. `done` gets the recording id.
    pub fn start_recording(
        &self,
        identifier: String,
        path: String,
        audio: Option<String>,
        done: impl FnOnce(&mut State, Reply) + 'static,
    ) {
        let state = self.clone();
        let evlh = self.0.evlh.clone();
        self.spawn(async move {
            let reply = start(&state, &identifier, &path, audio)
                .await
                .map_err(|err| err.to_string());
            evlh.insert_idle(move |state| done(state, reply));
        });
    }

    /// Finish the recording `id`. `done` gets the file's path.
    pub fn stop_recording(&self, id: String, done: impl FnOnce(&mut State, StopReply) + 'static) {
        let state = self.clone();
        let evlh = self.0.evlh.clone();
        self.spawn(async move {
            let reply = match state.session_conn().await {
                Ok(conn) => stop(conn, &id).await,
                Err(err) => Err(err.into()),
            };
            evlh.insert_idle(move |state| done(state, reply));
        });
    }

    /// The engine has no recording `id`: game mode stops listing it, quietly.
    pub fn forget_recording(&self, id: String) {
        if let Some(forget) = &*self.0.forget_recording.borrow() {
            let _ = forget.unbounded_send(id);
        }
    }
}

async fn start(
    state: &DBusState,
    identifier: &str,
    path: &str,
    audio: Option<String>,
) -> zbus::Result<String> {
    let conn = state.session_conn().await?;
    let proxy = CaptureEngine1Proxy::new(conn).await?;
    // A recording shows the pointer, as Kora Capture's do by default.
    let options = RecordOptions {
        codec: Some(Codec::Auto),
        cursor: true,
        audio,
        ..RecordOptions::default()
    };
    proxy
        .start_recording(&Source::toplevel(identifier), path, &options)
        .await
}

async fn stop(conn: &zbus::Connection, id: &str) -> StopReply {
    let proxy = CaptureEngine1Proxy::new(conn).await?;
    Ok(proxy.stop_recording(id).await?)
}

enum Heard {
    /// The engine started or stopped being there, or started a recording.
    Refresh,
    Stopped {
        id: String,
        path: String,
        error: String,
    },
    /// A stop found the engine without this recording.
    Forgotten(String),
}

/// What game mode shows after one event from the engine.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Mirrored {
    /// The earliest start and every id, or `None` when the engine did not answer.
    listed: Option<(u64, Vec<String>)>,
    /// Endings to announce: id (empty when `RecordingStopped` reaches the halo
    /// itself), path, and error.
    ended: Vec<(String, String, String)>,
}

/// Mirror the engine's recordings onto game mode: what it lists right now,
/// re-read each time it says anything or comes and goes, so what game mode
/// shows is the engine's own answer. Never starts it: the list is only read
/// while it has an owner, and it serves every recording it has.
pub(super) async fn watch_recordings(state: DBusState) -> Result<()> {
    let conn = state.session_conn().await?.clone();
    let (forget, forgotten) = futures_channel::mpsc::unbounded();
    *state.0.forget_recording.borrow_mut() = Some(forget);
    let evlh = state.0.evlh.clone();
    follow_recordings(&conn, forgotten, move |mirrored| {
        evlh.insert_idle(move |state| apply(state, mirrored));
        true
    })
    .await
}

/// Hand `emit` what game mode shows after each event, until it returns false.
async fn follow_recordings(
    conn: &zbus::Connection,
    forgotten: impl futures_util::Stream<Item = String> + Unpin,
    mut emit: impl FnMut(Mirrored) -> bool,
) -> Result<()> {
    let dbus = DBusProxy::new(conn).await.context("bus proxy")?;
    let proxy = CaptureEngine1Proxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .context("recorder proxy")?;
    // Subscribed first, so a change in between is not missed.
    let owners = dbus
        .receive_name_owner_changed_with_args(&[(0, BUS_NAME)])
        .await
        .context("subscribe to the recorder's name")?
        .map(|_| Heard::Refresh);
    let started = proxy
        .receive_recording_started()
        .await
        .context("subscribe to RecordingStarted")?
        .map(|_| Heard::Refresh);
    let stopped = proxy
        .receive_recording_stopped()
        .await
        .context("subscribe to RecordingStopped")?
        .filter_map(|signal| {
            std::future::ready(signal.args().ok().map(|args| Heard::Stopped {
                id: args.id.clone(),
                path: args.path.clone(),
                error: args.error.clone(),
            }))
        });
    let forgotten = forgotten.map(Heard::Forgotten);
    let mut events = Box::pin(select(owners, select(started, select(stopped, forgotten))));

    let mut known: HashMap<String, String> = HashMap::new();
    if !emit(mirror(&dbus, &proxy, Heard::Refresh, &mut known).await) {
        return Ok(());
    }
    while let Some(heard) = events.next().await {
        if !emit(mirror(&dbus, &proxy, heard, &mut known).await) {
            break;
        }
    }
    Ok(())
}

async fn mirror(
    dbus: &DBusProxy<'_>,
    proxy: &CaptureEngine1Proxy<'_>,
    heard: Heard,
    known: &mut HashMap<String, String>,
) -> Mirrored {
    let mut ended = Vec::new();
    match heard {
        Heard::Stopped { id, path, error } => {
            known.remove(&id);
            ended.push((String::new(), path, error));
        }
        // Its stop already told whoever asked; a failure here would be a second word.
        Heard::Forgotten(id) => {
            known.remove(&id);
        }
        Heard::Refresh => {}
    }
    let name = BusName::from_static_str(BUS_NAME).expect("the recorder's name is valid");
    // None when the engine is there but did not answer: nothing changes then.
    let listed = if dbus.name_has_owner(name).await.unwrap_or(false) {
        proxy.list_recordings().await.ok()
    } else {
        Some(Vec::new())
    };
    let snapshot = listed.map(|listed| {
        // What the engine dropped without a RecordingStopped went with its process.
        known.retain(|id, path| {
            let live = listed.iter().any(|recording| recording.id == *id);
            if !live {
                ended.push((id.clone(), path.clone(), "the recorder stopped".to_owned()));
            }
            live
        });
        for recording in &listed {
            known.insert(recording.id.clone(), recording.path.clone());
        }
        let since = listed.iter().map(|r| r.started).min().unwrap_or(0);
        (since, listed.into_iter().map(|r| r.id).collect::<Vec<_>>())
    });
    Mirrored {
        listed: snapshot,
        ended,
    }
}

fn apply(state: &mut State, Mirrored { listed, ended }: Mirrored) {
    if let Some((since, ids)) = listed {
        state.common.game_mode_bridge.set_recordings(since, ids);
    }
    for (id, path, error) in ended {
        if !id.is_empty() {
            crate::utils::recording::stopped(state, &id, &path, &error);
        }
        state.common.game_mode_bridge.notify_recording_ended(
            path,
            if error.is_empty() {
                String::new()
            } else {
                crate::utils::recording::failure_code(&error).to_owned()
            },
        );
    }
}

/// Follow `RecordingStopped` for as long as the session bus lives: every
/// recording ends through it, whether the halo stopped it, the window closed,
/// or the recorder failed. The match survives the recorder starting and
/// stopping; nothing here needs it to be running.
pub(super) async fn watch_stopped(state: DBusState) -> Result<()> {
    let conn = state.session_conn().await?.clone();
    let proxy = CaptureEngine1Proxy::new(&conn)
        .await
        .context("recorder proxy")?;
    let stream = proxy
        .receive_recording_stopped()
        .await
        .context("subscribe to RecordingStopped")?;
    let source = StreamSource::new(stream).unwrap();
    state
        .0
        .evlh
        .insert_source(source, |signal, _, state| {
            let Some(signal) = signal else {
                return;
            };
            let Ok(args) = signal.args() else {
                return;
            };
            debug!(id = %args.id, path = %args.path, error = %args.error, "recording stopped");
            crate::utils::recording::stopped(state, &args.id, &args.path, &args.error);
        })
        .map_err(|InsertError { error, .. }| error)
        .context("add RecordingStopped to the event loop")?;
    Ok(())
}

/// Follow another app's stills and recordings of a window (Kora Capture's), so
/// that window's halo plays them as if its own buttons had.
pub(super) async fn watch_windows(state: DBusState) -> Result<()> {
    let conn = state.session_conn().await?.clone();
    let proxy = CaptureEngine1Proxy::new(&conn)
        .await
        .context("recorder proxy")?;
    let captured = proxy
        .receive_window_captured()
        .await
        .context("subscribe to WindowCaptured")?;
    let started = proxy
        .receive_window_recording_started()
        .await
        .context("subscribe to WindowRecordingStarted")?;
    let evlh = &state.0.evlh;
    evlh.insert_source(StreamSource::new(captured).unwrap(), |signal, _, state| {
        let Some(Ok(args)) = signal.as_ref().map(|s| s.args()) else {
            return;
        };
        debug!(window = %args.identifier, "window captured");
        crate::utils::screenshot::captured_elsewhere(state, &args.identifier);
    })
    .map_err(|InsertError { error, .. }| error)
    .context("add WindowCaptured to the event loop")?;
    evlh.insert_source(StreamSource::new(started).unwrap(), |signal, _, state| {
        let Some(Ok(args)) = signal.as_ref().map(|s| s.args()) else {
            return;
        };
        debug!(id = %args.id, window = %args.identifier, "window recording started");
        crate::utils::recording::started_elsewhere(state, &args.id, &args.identifier);
    })
    .map_err(|InsertError { error, .. }| error)
    .context("add WindowRecordingStarted to the event loop")?;
    Ok(())
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
