// SPDX-License-Identifier: GPL-3.0-only

//! Toasts through the freedesktop `org.freedesktop.Notifications` interface,
//! and system toasts and capture previews through kora-notifications' own
//! client crate, sent over the compositor's session-bus connection.

use std::collections::HashMap;

use futures_channel::mpsc::UnboundedSender;
use futures_util::StreamExt;
use kora_notifications_client::Notifications1Proxy;
use tracing::{debug, warn};
use zbus::zvariant::Value;

use super::DBusState;

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, &Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

/// The colour grammar a system toast's dot carries, as kora-notifications
/// names it. A system toast is a one-line confirmation of something the user
/// just did, never a notification (no history, no unread, no sound).
pub use kora_notifications_client::Tone;

/// One toast. `app_icon` is a freedesktop icon name.
#[derive(Debug, Clone)]
pub struct Notification {
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    /// Milliseconds before the daemon dismisses the toast on its own.
    pub expire_timeout: i32,
    /// A transient toast is shown once and kept out of the history.
    pub transient: bool,
    /// What activating it opens.
    pub target: Option<Target>,
}

/// A folder or file a notification opens, in kora-notifications' hints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `folder` or `file`.
    pub kind: &'static str,
    /// The path as apps in `workspace` name it.
    pub path: String,
    pub name: String,
    /// The Kora workspace it opens in; `None` on the machine.
    pub workspace: Option<String>,
    /// The row button's words.
    pub label: String,
}

impl DBusState {
    /// Send a toast, fire and forget. A missing daemon is only logged.
    pub fn notify(&self, notification: Notification) {
        let state = self.clone();
        self.spawn(async move {
            if let Err(err) = send(&state, &notification).await {
                warn!(?err, summary = %notification.summary, "Failed to send notification");
            }
        });
    }
}

/// Whether the shell confirms with system toasts rather than notifications.
/// `COSMIC_SYSTEM_TOASTS` decides; unset, toasts follow `COSMIC_WORKSPACES`.
pub fn toasts_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        crate::utils::env::bool_var("COSMIC_SYSTEM_TOASTS")
            .unwrap_or_else(super::workspaces::enabled)
    })
}

impl DBusState {
    /// Confirm something the user just did, as a toast or a plain notification.
    pub fn system_toast(&self, message: String, tone: Tone) {
        let notification = Notification {
            app_name: system_name(),
            app_icon: String::new(),
            summary: plain(message.clone()),
            body: String::new(),
            expire_timeout: 5000,
            transient: true,
            target: None,
        };
        self.system_toast_or(message, tone, notification);
    }

    /// A system toast when [`toasts_enabled`], otherwise `notification`.
    pub fn system_toast_or(&self, message: String, tone: Tone, notification: Notification) {
        if !toasts_enabled() {
            self.notify(notification);
            return;
        }
        let message = plain(message);
        let state = self.clone();
        self.spawn(async move {
            let sent = async {
                let conn = state.session_conn().await?;
                Notifications1Proxy::new(conn)
                    .await?
                    .toast(&message, tone)
                    .await
            };
            if let Err(err) = sent.await {
                warn!(?err, %message, "Failed to show a system toast");
            }
        });
    }
}

impl DBusState {
    /// Hand a screenshot or recording to the shell's preview above the dock.
    /// `unseen` runs instead when nothing previews it, so the file is not lost.
    pub fn show_capture(
        &self,
        handoff: crate::utils::captures::Handoff,
        unseen: impl FnOnce(&mut crate::state::State, crate::utils::captures::Handoff) + 'static,
    ) {
        if !toasts_enabled() {
            let evlh = self.0.evlh.clone();
            evlh.insert_idle(move |state| unseen(state, handoff));
            return;
        }
        let state = self.clone();
        let evlh = self.0.evlh.clone();
        self.spawn(async move {
            let sent = async {
                let conn = state.session_conn().await?;
                Notifications1Proxy::new(conn)
                    .await?
                    .show_capture(&handoff.capture)
                    .await
            };
            if let Err(err) = sent.await {
                warn!(?err, "Nothing previewed the capture; filing it");
                evlh.insert_idle(move |state| unseen(state, handoff));
            }
        });
    }
}

impl DBusState {
    /// Tell the preview, in order, which app a dragged capture is over (`""` for none).
    /// The channel closes with the drag.
    pub fn capture_drag_targets(&self) -> UnboundedSender<String> {
        let (tell, mut heard) = futures_channel::mpsc::unbounded::<String>();
        if !toasts_enabled() {
            return tell;
        }
        let state = self.clone();
        self.spawn(async move {
            let Ok(conn) = state.session_conn().await else {
                return;
            };
            let Ok(preview) = Notifications1Proxy::new(conn).await else {
                return;
            };
            while let Some(mut app) = heard.next().await {
                // Only the latest name matters once several are waiting.
                while let Ok(newer) = heard.try_recv() {
                    app = newer;
                }
                if let Err(err) = preview.capture_drag_over(&app).await {
                    debug!(?err, "The preview did not take the drag's target");
                }
            }
        });
        tell
    }
}

/// The system's own name, from os-release, so each distribution names itself.
fn system_name() -> String {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        ["/etc/os-release", "/usr/lib/os-release"]
            .iter()
            .find_map(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| os_release_name(&text))
            .unwrap_or_else(|| crate::fl!("shell-notification-app"))
    })
    .clone()
}

fn os_release_name(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("NAME="))
        .map(|value| value.trim().trim_matches('"').to_owned())
        .filter(|value| !value.is_empty())
}

/// Fluent's bidi isolate marks around a placeable surface as tofu in the
/// toast's Latin-only fonts.
pub fn plain(text: String) -> String {
    text.replace(['\u{2068}', '\u{2069}'], "")
}

async fn send(state: &DBusState, notification: &Notification) -> zbus::Result<()> {
    let conn = state.session_conn().await?;
    let proxy = NotificationsProxy::new(conn).await?;
    let hints = hints(notification);
    proxy
        .notify(
            &notification.app_name,
            0,
            &notification.app_icon,
            &notification.summary,
            &notification.body,
            &[],
            hints.iter().map(|(key, value)| (*key, value)).collect(),
            notification.expire_timeout,
        )
        .await?;
    Ok(())
}

/// The hints `Notify` takes for `notification`.
fn hints(notification: &Notification) -> Vec<(&'static str, Value<'_>)> {
    let mut hints = vec![("transient", Value::Bool(notification.transient))];
    let Some(target) = &notification.target else {
        return hints;
    };
    hints.extend([
        ("x-kora-target-kind", Value::from(target.kind)),
        ("x-kora-target-id", Value::from(target.path.as_str())),
        ("x-kora-target-name", Value::from(target.name.as_str())),
        ("x-kora-action-label", Value::from(target.label.as_str())),
    ]);
    if let Some(workspace) = &target.workspace {
        hints.push(("x-kora-workspace", Value::from(workspace.as_str())));
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fl;

    #[test]
    fn a_target_becomes_kora_notifications_hints() {
        let mut notification = Notification {
            app_name: "Recording".into(),
            app_icon: String::new(),
            summary: "Recording failed".into(),
            body: String::new(),
            expire_timeout: -1,
            transient: false,
            target: None,
        };
        assert_eq!(hints(&notification).len(), 1);
        notification.target = Some(Target {
            kind: "folder",
            path: "/home/u/Captures/Recordings".into(),
            name: "Recordings".into(),
            workspace: Some("work".into()),
            label: "Open folder".into(),
        });
        let hints: HashMap<_, _> = hints(&notification).into_iter().collect();
        let text = |key: &str| String::try_from(hints[key].clone()).unwrap();
        assert_eq!(text("x-kora-target-kind"), "folder");
        assert_eq!(text("x-kora-target-id"), "/home/u/Captures/Recordings");
        assert_eq!(text("x-kora-workspace"), "work");
        assert_eq!(hints["transient"], Value::Bool(false));
    }

    #[test]
    fn the_system_names_itself_from_os_release() {
        let text = "PRETTY_NAME=\"Example OS 1\"\nNAME=\"Example OS\"\nID=example\n";
        assert_eq!(os_release_name(text).as_deref(), Some("Example OS"));
        assert_eq!(os_release_name("NAME=Plain\n").as_deref(), Some("Plain"));
        assert_eq!(os_release_name("NAME=\"\"\n"), None);
        assert_eq!(os_release_name("ID=example\n"), None);
    }

    #[test]
    fn the_shells_toasts_read_as_the_prototype_writes_them() {
        assert_eq!(fl!("screenshot-saved"), "Screenshot saved");
        assert_eq!(
            fl!("recording-stopped-toast"),
            "Recording stopped — saved with provenance"
        );
        assert_eq!(
            plain(fl!("halo-one-window", app = "Files")),
            "Files opens one window"
        );
    }
}
