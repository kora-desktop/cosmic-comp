// SPDX-License-Identifier: GPL-3.0-only

//! `inc.kora.WindowPlaces1`: where each app's windows are, and where its next ones open.
//! An app that closes windows to open them again (System Updates) reads their places first
//! and hands them back before it reopens them.

use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
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

/// The one program these places are for: Settings, which closes apps for an update and
/// opens them again, from its window or from `kora-settings --reopen` after a restart.
const SETTINGS: &str = "kora-settings";

/// Why a caller was refused, for the log.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    NoProcess,
    InWorkspace(String),
    InSandbox,
    NotSettings,
}

/// What decides a caller, read from `/proc` once per call.
#[derive(Debug)]
struct Caller {
    /// The Kora workspace its cgroup puts it in.
    workspace: Option<String>,
    /// Whether it shares the compositor's user namespace, as nothing in a sandbox does.
    our_user_namespace: bool,
    /// Whether its executable is the same file as Settings' (device and inode, so a path
    /// bound over another file in a private mount does not pass).
    is_settings: bool,
}

impl Caller {
    fn read(pid: u32, settings: &[PathBuf]) -> Option<Self> {
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        let exe = std::fs::metadata(format!("/proc/{pid}/exe")).ok()?;
        let user_namespace = |pid: &str| {
            std::fs::metadata(format!("/proc/{pid}/ns/user"))
                .ok()
                .map(|ns| (ns.dev(), ns.ino()))
        };
        let theirs = user_namespace(&pid.to_string())?;
        Some(Self {
            workspace: crate::workspace_tag::of_cgroup(&cgroup),
            our_user_namespace: user_namespace("self") == Some(theirs),
            is_settings: settings.iter().any(|path| {
                std::fs::metadata(path)
                    .is_ok_and(|file| file.dev() == exe.dev() && file.ino() == exe.ino())
            }),
        })
    }

    fn allowed(&self) -> Result<(), Refusal> {
        if let Some(workspace) = &self.workspace {
            return Err(Refusal::InWorkspace(workspace.clone()));
        }
        if !self.our_user_namespace {
            return Err(Refusal::InSandbox);
        }
        if !self.is_settings {
            return Err(Refusal::NotSettings);
        }
        Ok(())
    }
}

/// The verdict on the process behind `pid`, and behind `pidfd` when the bus gave one: the
/// pidfd keeps a pid that was reused since the call from passing as its new owner.
fn verdict(pid: Option<u32>, pidfd: Option<RawFd>, settings: &[PathBuf]) -> Result<(), Refusal> {
    let pid = pid.ok_or(Refusal::NoProcess)?;
    let caller = Caller::read(pid, settings).ok_or(Refusal::NoProcess)?;
    if let Some(fd) = pidfd {
        let named = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
            .ok()
            .and_then(|info| crate::dbus::game_mode::pid_in_fdinfo(&info));
        if named != Some(pid) {
            return Err(Refusal::NoProcess);
        }
    }
    caller.allowed()
}

/// Ask the bus who `sender` is, on a connection of its own: asking from inside a method
/// handler on the serving one deadlocks the interface.
async fn check(sender: String) -> Result<(), Refusal> {
    let credentials = async {
        let conn = zbus::Connection::session().await.ok()?;
        let proxy = zbus::fdo::DBusProxy::new(&conn).await.ok()?;
        let name = zbus::names::BusName::try_from(sender).ok()?;
        proxy.get_connection_credentials(name).await.ok()
    }
    .await
    .ok_or(Refusal::NoProcess)?;
    verdict(
        credentials.process_id(),
        credentials.process_fd().map(AsRawFd::as_raw_fd),
        &crate::utils::process::which(SETTINGS),
    )
}

struct Interface {
    cmd: Mutex<Sender<Command>>,
    executor: ThreadPool,
}

impl Interface {
    /// Only Settings outside every workspace may read or set places.
    async fn allow(
        &self,
        header: &zbus::message::Header<'_>,
        method: &str,
    ) -> zbus::fdo::Result<()> {
        let sender = header.sender().map(ToString::to_string).unwrap_or_default();
        let (done, answer) = oneshot::channel();
        let asked = sender.clone();
        self.executor.spawn_ok(async move {
            let _ = done.send(check(asked).await);
        });
        match answer.await.unwrap_or(Err(Refusal::NoProcess)) {
            Ok(()) => Ok(()),
            Err(why) => {
                warn!(%sender, ?why, "Refused {NAME}.{method}: only Settings may call it");
                Err(zbus::fdo::Error::AccessDenied(format!(
                    "only Settings may call {NAME}"
                )))
            }
        }
    }

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
    async fn places(
        &self,
        workspace: String,
        apps: Vec<String>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<Vec<Fields>> {
        self.allow(&header, "Places").await?;
        let (reply, answer) = oneshot::channel();
        self.send(Command::Places {
            workspace: workspace_of(workspace),
            apps,
            reply,
        })?;
        let places = answer.await.unwrap_or_default();
        Ok(places.iter().map(Place::fields).collect())
    }

    async fn expect(
        &self,
        workspace: String,
        places: Vec<Fields>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.allow(&header, "Expect").await?;
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
        executor: executor.clone(),
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

    fn caller(workspace: Option<&str>, our_user_namespace: bool, is_settings: bool) -> Caller {
        Caller {
            workspace: workspace.map(Into::into),
            our_user_namespace,
            is_settings,
        }
    }

    #[test]
    fn only_settings_outside_every_workspace_may_call() {
        assert_eq!(caller(None, true, true).allowed(), Ok(()));
        assert_eq!(
            caller(Some("teal"), true, true).allowed(),
            Err(Refusal::InWorkspace("teal".into())),
            "Settings' binary run inside a workspace"
        );
        assert_eq!(caller(None, false, true).allowed(), Err(Refusal::InSandbox));
        assert_eq!(
            caller(None, true, false).allowed(),
            Err(Refusal::NotSettings),
            "another machine program"
        );
    }

    #[test]
    fn a_caller_is_settings_only_when_its_executable_is_the_same_file() {
        let pid = std::process::id();
        let us = std::env::current_exe().unwrap();
        let read = Caller::read(pid, std::slice::from_ref(&us)).unwrap();
        assert!(read.is_settings && read.our_user_namespace);
        let other = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let read = Caller::read(pid, &[other, PathBuf::from("/nonexistent/kora-settings")]);
        assert!(!read.unwrap().is_settings);
        assert!(
            verdict(Some(pid), None, &[]).is_err(),
            "with no Settings installed nobody may call"
        );
    }

    #[test]
    fn an_unknown_or_reused_pid_is_refused() {
        let us = [std::env::current_exe().unwrap()];
        assert_eq!(verdict(None, None, &us), Err(Refusal::NoProcess));
        assert_eq!(verdict(Some(u32::MAX), None, &us), Err(Refusal::NoProcess));
        let pid = std::process::id();
        let pidfd = rustix::process::pidfd_open(
            rustix::process::Pid::from_raw(pid as i32).unwrap(),
            rustix::process::PidfdFlags::empty(),
        )
        .unwrap();
        let parent = std::os::unix::process::parent_id();
        assert_eq!(
            verdict(Some(parent), Some(pidfd.as_raw_fd()), &us),
            Err(Refusal::NoProcess),
            "the bus's pid no longer names the process that called"
        );
        assert_ne!(
            verdict(Some(pid), Some(pidfd.as_raw_fd()), &us),
            Err(Refusal::NoProcess)
        );
    }

    #[test]
    fn a_place_carries_no_title() {
        let mut keys: Vec<String> = place("files", 1).fields().into_keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "app",
                "desktop",
                "height",
                "maximized",
                "output",
                "width",
                "x",
                "y"
            ]
        );
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
