// SPDX-License-Identifier: GPL-3.0-only

//! `inc.kora.WindowPlaces1`: where each app's windows are, and where its next ones open.
//! An app that closes windows to open them again (System Updates) reads their places first
//! and hands them back before it reopens them.

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use calloop::{LoopHandle, channel::Sender};
use futures_channel::oneshot;
use futures_executor::ThreadPool;
use kora_window_places_client::{EXPECT_FOR, Fields, MAX_PLACES, NAME, PATH, Place};
use tracing::{debug, warn};

use crate::state::State;

pub enum Command {
    Places {
        workspace: Option<String>,
        apps: Vec<String>,
        reply: oneshot::Sender<Vec<Place>>,
    },
    Expect {
        workspace: Option<String>,
        places: Vec<Place>,
    },
}

/// Places handed over and not taken yet, each with its Kora workspace (`None` for the machine).
#[derive(Debug, Default)]
pub struct Expected {
    places: Vec<(Option<String>, Place, Instant)>,
}

impl Expected {
    /// A new hand-over for an app replaces what was left of its last one.
    pub fn add(&mut self, workspace: Option<String>, places: Vec<Place>, now: Instant) {
        self.forget_old(now);
        self.places
            .retain(|(at, old, _)| *at != workspace || !places.iter().any(|p| p.app == old.app));
        let room = (MAX_PLACES * 4).saturating_sub(self.places.len());
        self.places.extend(
            places
                .into_iter()
                .take(MAX_PLACES.min(room))
                .map(|place| (workspace.clone(), place, now)),
        );
    }

    /// The first place left for a new window of `app` in `workspace`.
    pub fn take(&mut self, workspace: Option<&str>, app: &str, now: Instant) -> Option<Place> {
        self.forget_old(now);
        let at = self
            .places
            .iter()
            .position(|(at, place, _)| at.as_deref() == workspace && place.app == app)?;
        Some(self.places.remove(at).1)
    }

    fn forget_old(&mut self, now: Instant) {
        self.places
            .retain(|(_, _, since)| now.saturating_duration_since(*since) < EXPECT_FOR);
    }
}

fn workspace_of(workspace: String) -> Option<String> {
    (!workspace.is_empty()).then_some(workspace)
}

struct Interface {
    cmd: Mutex<Sender<Command>>,
}

impl Interface {
    fn send(&self, cmd: Command) -> zbus::fdo::Result<()> {
        self.cmd
            .lock()
            .unwrap()
            .send(cmd)
            .map_err(|_| zbus::fdo::Error::Failed("the compositor is stopping".into()))
    }
}

#[zbus::interface(name = "inc.kora.WindowPlaces1")]
impl Interface {
    async fn places(&self, workspace: String, apps: Vec<String>) -> zbus::fdo::Result<Vec<Fields>> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Places {
            workspace: workspace_of(workspace),
            apps,
            reply,
        })?;
        let places = answer.await.unwrap_or_default();
        Ok(places.iter().map(Place::fields).collect())
    }

    async fn expect(&self, workspace: String, places: Vec<Fields>) -> zbus::fdo::Result<()> {
        let places = places.iter().filter_map(Place::read).collect();
        self.send(Command::Expect {
            workspace: workspace_of(workspace),
            places,
        })
    }
}

/// Kept for the session: dropping it would give the name up.
static CONNECTION: OnceLock<zbus::Connection> = OnceLock::new();

pub fn init(handle: &LoopHandle<'static, State>, executor: &ThreadPool) {
    let (tx, rx) = calloop::channel::channel();
    if let Err(err) = handle.insert_source(rx, |event, _, state| {
        if let calloop::channel::Event::Msg(cmd) = event {
            state.handle_window_places(cmd);
        }
    }) {
        warn!(?err, "Failed to register the window places channel");
        return;
    }
    let interface = Interface {
        cmd: Mutex::new(tx),
    };
    executor.spawn_ok(async move {
        let served = async {
            let conn = zbus::connection::Builder::session()?
                .serve_at(PATH, interface)?
                .build()
                .await?;
            conn.request_name(NAME).await?;
            zbus::Result::Ok(conn)
        };
        match served.await {
            Ok(conn) => {
                let _ = CONNECTION.set(conn);
                debug!("Serving {NAME}");
            }
            Err(err) => warn!(
                ?err,
                "Failed to serve {NAME}; apps reopen where new windows go"
            ),
        }
    });
}

impl State {
    pub fn handle_window_places(&mut self, cmd: Command) {
        match cmd {
            Command::Places {
                workspace,
                apps,
                reply,
            } => {
                let places = self
                    .common
                    .shell
                    .read()
                    .window_places(workspace.as_deref(), &apps);
                let _ = reply.send(places);
            }
            Command::Expect { workspace, places } => {
                self.common
                    .shell
                    .write()
                    .expected_places
                    .add(workspace, places, Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn place(app: &str, x: i32) -> Place {
        Place {
            app: app.into(),
            output: "eDP-1".into(),
            x,
            width: 800,
            height: 600,
            ..Place::default()
        }
    }

    #[test]
    fn each_window_takes_its_apps_next_place_once() {
        let now = Instant::now();
        let mut expected = Expected::default();
        let ws = Some("teal".to_string());
        expected.add(ws.clone(), vec![place("files", 1), place("files", 2)], now);
        assert_eq!(
            expected.take(None, "files", now),
            None,
            "another workspace's"
        );
        assert_eq!(
            expected.take(Some("teal"), "files", now).map(|p| p.x),
            Some(1)
        );
        assert_eq!(
            expected.take(Some("teal"), "files", now).map(|p| p.x),
            Some(2)
        );
        assert_eq!(expected.take(Some("teal"), "files", now), None);
    }

    #[test]
    fn places_expire_and_a_new_hand_over_replaces_the_old() {
        let now = Instant::now();
        let mut expected = Expected::default();
        expected.add(None, vec![place("files", 1), place("term", 5)], now);
        expected.add(None, vec![place("files", 3)], now);
        assert_eq!(expected.take(None, "files", now).map(|p| p.x), Some(3));
        assert_eq!(expected.take(None, "files", now), None);
        let later = now + EXPECT_FOR + Duration::from_secs(1);
        assert_eq!(expected.take(None, "term", later), None, "expired");
    }
}
