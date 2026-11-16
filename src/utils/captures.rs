//! Where a window's captures go, and what they are called.
//!
//! A capture is saved into the home of the workspace the window belongs to —
//! `~/Workspaces/<id>` on the machine plane, which the sandbox binds onto
//! `$HOME` — so it shows up in that workspace's own files. Outside a workspace
//! it is the user's home. One environment variable per kind moves it.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use kora_notifications_client as preview;

use crate::{shell::element::CosmicSurface, state::State};

/// Same variable and values as kora-workspaces' `DefaultRoot`: `home`/`HOME`
/// roots the default workspace at `$HOME` itself; anything else, unset
/// included, at `~/Workspaces/default`.
const DEFAULT_ROOT_ENV: &str = "KORA_DEFAULT_WORKSPACE_ROOT";
const WORKSPACES_DIR: &str = "Workspaces";
const DEFAULT_WORKSPACE: &str = "default";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureKind {
    Screenshot,
    Recording,
}

impl CaptureKind {
    /// The variable that moves this kind of capture. Relative, it is taken
    /// under the window's home; absolute, every capture lands in that one place.
    pub const fn env(self) -> &'static str {
        match self {
            CaptureKind::Screenshot => "COSMIC_SCREENSHOT_DIR",
            CaptureKind::Recording => "COSMIC_RECORDING_DIR",
        }
    }

    const fn default_dir(self) -> &'static str {
        match self {
            CaptureKind::Screenshot => "Captures/Screenshots",
            CaptureKind::Recording => "Captures/Recordings",
        }
    }

    /// What a file of this kind is called before its time.
    const fn noun(self) -> &'static str {
        match self {
            CaptureKind::Screenshot => "Screenshot",
            CaptureKind::Recording => "Screen Recording",
        }
    }

    pub const fn preview_kind(self) -> preview::CaptureKind {
        match self {
            CaptureKind::Screenshot => preview::CaptureKind::Image,
            CaptureKind::Recording => preview::CaptureKind::Video,
        }
    }
}

/// A recording's name while it is being written, so nothing takes it for a whole file.
const PARTIAL: &str = "part";

pub fn default_root_is_home(value: Option<&str>) -> bool {
    matches!(value, Some("home" | "HOME"))
}

/// The home a window's files live in, seen from the machine plane: its
/// workspace's root, or the user's home outside a workspace.
pub fn workspace_home(home: &Path, workspace: Option<&str>, default_root_is_home: bool) -> PathBuf {
    match workspace {
        None => home.to_path_buf(),
        Some(DEFAULT_WORKSPACE) if default_root_is_home => home.to_path_buf(),
        Some(id) => home.join(WORKSPACES_DIR).join(id),
    }
}

/// Where a capture of `kind` for a window in `workspace` goes, with
/// `configured` being the kind's environment variable.
pub fn directory(
    kind: CaptureKind,
    home: &Path,
    workspace: Option<&str>,
    default_root_is_home: bool,
    configured: Option<&OsStr>,
) -> PathBuf {
    let dir = configured
        .filter(|dir| !dir.is_empty())
        .map_or_else(|| PathBuf::from(kind.default_dir()), PathBuf::from);
    if dir.is_absolute() {
        dir
    } else {
        workspace_home(home, workspace, default_root_is_home).join(dir)
    }
}

/// The workspace a capture of `surface` belongs to. A machine-plane window
/// has none of its own; its capture goes where the user is standing.
fn workspace_for(state: &State, surface: &CosmicSurface) -> Option<String> {
    let shell = state.common.shell.read();
    shell
        .client_workspace(surface)
        .or_else(|| shell.active_workspace().map(ToString::to_string))
}

/// The directory for a capture of `surface`, from the environment and the
/// workspace registry. `None` without a `HOME`.
pub fn directory_for(state: &State, surface: &CosmicSurface, kind: CaptureKind) -> Option<PathBuf> {
    let workspace = workspace_for(state, surface);
    let home = std::env::var_os("HOME")?;
    Some(directory(
        kind,
        Path::new(&home),
        workspace.as_deref(),
        default_root_is_home(std::env::var(DEFAULT_ROOT_ENV).ok().as_deref()),
        std::env::var_os(kind.env()).as_deref(),
    ))
}

/// Everything a capture's preview needs to know besides the file itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Destination {
    pub kind: CaptureKind,
    /// Where the file is written and stays.
    pub save_to: PathBuf,
    /// Where a recording's poster waits for the preview, outside the home.
    pub posters: PathBuf,
    /// The window's home on the machine, and the home apps there see it as.
    pub home: PathBuf,
    pub seen_as: PathBuf,
    pub workspace: Option<String>,
}

impl Destination {
    pub fn new(
        kind: CaptureKind,
        home: &Path,
        workspace: Option<&str>,
        default_root_is_home: bool,
        configured: Option<&OsStr>,
    ) -> Self {
        let window_home = workspace_home(home, workspace, default_root_is_home);
        Self {
            kind,
            save_to: directory(kind, home, workspace, default_root_is_home, configured),
            posters: std::env::var_os("XDG_RUNTIME_DIR")
                .map_or_else(std::env::temp_dir, PathBuf::from)
                .join("cosmic-comp-posters"),
            seen_as: home.to_path_buf(),
            home: window_home,
            workspace: workspace.map(str::to_string),
        }
    }

    pub fn for_surface(state: &State, surface: &CosmicSurface, kind: CaptureKind) -> Option<Self> {
        let workspace = workspace_for(state, surface);
        let home = std::env::var_os("HOME")?;
        Some(Self::new(
            kind,
            Path::new(&home),
            workspace.as_deref(),
            default_root_is_home(std::env::var(DEFAULT_ROOT_ENV).ok().as_deref()),
            std::env::var_os(kind.env()).as_deref(),
        ))
    }

    /// A new recording's path while it is written: `<name>.mp4.part`, under a free name.
    pub fn partial(&self, time: &jiff::Zoned) -> PathBuf {
        let stem = capture_stem(self.kind, time);
        let ext = "mp4";
        std::iter::once(stem.clone())
            .chain((2..100).map(|n| format!("{stem} ({n})")))
            .map(|name| self.save_to.join(format!("{name}.{ext}.{PARTIAL}")))
            .find(|p| !p.exists() && !p.with_extension("").exists())
            .unwrap_or_else(|| self.save_to.join(format!("{stem}.{ext}.{PARTIAL}")))
    }

    /// The poster of the recording at `recording`.
    pub fn poster_of(&self, recording: &Path) -> PathBuf {
        let name = recording.file_name().unwrap_or_default();
        self.posters.join(name).with_extension("poster.png")
    }

    /// `path` as apps in the window's workspace name it: the sandbox mounts
    /// the workspace's home as the user's.
    pub fn seen_inside(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(&self.home) {
            Ok(rest) => self.seen_as.join(rest),
            Err(_) => path.to_path_buf(),
        }
    }

    /// What the preview's `ShowCapture` is told about the file at `path`.
    pub fn handoff(
        &self,
        path: &Path,
        app: &str,
        poster: Option<&Path>,
        duration_ms: Option<u64>,
    ) -> Handoff {
        let text = |p: &Path| p.to_string_lossy().into_owned();
        Handoff {
            path: path.to_path_buf(),
            save_to: self.save_to.clone(),
            capture: preview::Capture {
                path: text(path),
                uri: Some(file_uri(&self.seen_inside(path))),
                kind: self.kind.preview_kind(),
                app: Some(app.to_string()),
                save_to: text(&self.save_to),
                downloads: Some(text(&self.home.join("Downloads"))),
                workspace: self.workspace.clone(),
                poster: poster.map(text),
                duration_ms,
            },
        }
    }
}

/// A capture on its way to the preview, and where the file goes if nobody
/// previews it.
#[derive(Debug, Clone, PartialEq)]
pub struct Handoff {
    pub path: PathBuf,
    pub save_to: PathBuf,
    pub capture: preview::Capture,
}

/// `file://` and the path, percent-encoded where a URI needs it.
pub fn file_uri(path: &Path) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("file://");
    for byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(*byte));
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// A finished recording's `.part` renamed to a name not taken; other paths as they are.
pub fn finish_partial(path: &Path) -> std::io::Result<PathBuf> {
    if path.extension().and_then(OsStr::to_str) != Some(PARTIAL) {
        return Ok(path.to_path_buf());
    }
    let whole = path.with_extension("");
    let Some(dir) = whole.parent() else {
        return Ok(path.to_path_buf());
    };
    let to = free_name(dir, &whole)?;
    std::fs::rename(path, &to)?;
    Ok(to)
}

/// `path`, or `name (n).ext` beside it, whichever is not taken.
fn free_name(dir: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy()
        .into_owned();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    std::iter::once(dir.join(&name))
        .chain((2..100).map(|n| dir.join(format!("{stem} ({n}){ext}"))))
        .find(|p| !p.exists())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free name"))
}

/// Move `path` into `dir` under a name not taken there; the new path. A file
/// already in `dir` stays as it is.
pub fn file_into(path: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    if path.parent() == Some(dir) {
        return Ok(path.to_path_buf());
    }
    std::fs::create_dir_all(dir)?;
    let to = free_name(dir, path)?;
    if std::fs::rename(path, &to).is_err() {
        std::fs::copy(path, &to)?;
        std::fs::remove_file(path)?;
    }
    Ok(to)
}

/// `Screenshot 2026-09-10 at 14.25.30`, as a capture is named.
pub fn capture_stem(kind: CaptureKind, time: &jiff::Zoned) -> String {
    format!("{} {}", kind.noun(), time.strftime("%Y-%m-%d at %H.%M.%S"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: i8, minute: i8, second: i8) -> jiff::Zoned {
        jiff::civil::date(2026, 9, 10)
            .at(hour, minute, second, 0)
            .in_tz("UTC")
            .unwrap()
    }

    #[test]
    fn captures_land_in_the_window_workspace_home() {
        let home = Path::new("/home/u");
        let dir = |kind, workspace, root_is_home, env: Option<&str>| {
            directory(kind, home, workspace, root_is_home, env.map(OsStr::new))
        };
        assert_eq!(
            dir(CaptureKind::Screenshot, None, false, None),
            Path::new("/home/u/Captures/Screenshots")
        );
        assert_eq!(
            dir(CaptureKind::Recording, Some("meridian"), false, None),
            Path::new("/home/u/Workspaces/meridian/Captures/Recordings")
        );
        assert_eq!(
            dir(CaptureKind::Screenshot, Some("default"), false, None),
            Path::new("/home/u/Workspaces/default/Captures/Screenshots")
        );
        assert_eq!(
            dir(CaptureKind::Screenshot, Some("default"), true, None),
            Path::new("/home/u/Captures/Screenshots")
        );
        assert!(default_root_is_home(Some("home")));
        assert!(default_root_is_home(Some("HOME")));
        assert!(!default_root_is_home(Some("workspaces")));
        assert!(!default_root_is_home(None));
    }

    #[test]
    fn the_directory_variable_moves_captures() {
        let home = Path::new("/home/u");
        let dir =
            |kind, workspace, env| directory(kind, home, workspace, false, Some(OsStr::new(env)));
        assert_eq!(
            dir(CaptureKind::Recording, Some("meridian"), "Clips"),
            Path::new("/home/u/Workspaces/meridian/Clips")
        );
        assert_eq!(
            dir(CaptureKind::Screenshot, Some("meridian"), "/srv/shots"),
            Path::new("/srv/shots")
        );
        assert_eq!(
            dir(CaptureKind::Screenshot, None, ""),
            Path::new("/home/u/Captures/Screenshots")
        );
        assert_eq!(CaptureKind::Recording.env(), "COSMIC_RECORDING_DIR");
    }

    #[test]
    fn a_capture_is_saved_in_its_workspace_and_named_there_as_its_apps_see_it() {
        let home = Path::new("/home/u");
        let dest = Destination::new(CaptureKind::Recording, home, Some("work"), false, None);
        assert_eq!(
            dest.save_to,
            Path::new("/home/u/Workspaces/work/Captures/Recordings")
        );
        let file = dest.save_to.join("Screen Recording 1.mp4");
        assert_eq!(
            dest.seen_inside(&file),
            Path::new("/home/u/Captures/Recordings/Screen Recording 1.mp4")
        );
        let capture = dest.handoff(&file, "Slate", None, Some(5400)).capture;
        assert_eq!(
            capture.uri.as_deref(),
            Some("file:///home/u/Captures/Recordings/Screen%20Recording%201.mp4")
        );
        assert_eq!(
            capture.downloads.as_deref(),
            Some("/home/u/Workspaces/work/Downloads")
        );
        assert_eq!(capture.workspace.as_deref(), Some("work"));
        assert_eq!(capture.kind, preview::CaptureKind::Video);
        assert_eq!(capture.duration_ms, Some(5400));

        let outside = Destination::new(CaptureKind::Screenshot, home, None, false, None);
        assert_eq!(outside.seen_inside(&outside.save_to), outside.save_to);
    }

    // The preview and the Captures folder never show a recording still being written.
    #[test]
    fn a_recording_is_written_as_a_part_file_and_named_once_it_ends() {
        let dir = std::env::temp_dir().join(format!("cosmic-comp-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut dest = Destination::new(CaptureKind::Recording, &dir, None, false, None);
        dest.posters = dir.join("posters");
        std::fs::create_dir_all(&dest.save_to).unwrap();
        std::fs::write(
            dest.save_to
                .join("Screen Recording 2026-09-10 at 14.25.30.mp4"),
            b"older",
        )
        .unwrap();
        let partial = dest.partial(&at(14, 25, 30));
        assert_eq!(
            partial,
            dest.save_to
                .join("Screen Recording 2026-09-10 at 14.25.30 (2).mp4.part")
        );
        assert_eq!(
            dest.poster_of(&partial),
            dir.join("posters/Screen Recording 2026-09-10 at 14.25.30 (2).mp4.poster.png")
        );
        std::fs::write(&partial, b"video").unwrap();
        let done = finish_partial(&partial).unwrap();
        assert_eq!(
            done,
            dest.save_to
                .join("Screen Recording 2026-09-10 at 14.25.30 (2).mp4")
        );
        assert!(done.is_file() && !partial.exists());
        assert_eq!(
            finish_partial(&done).unwrap(),
            done,
            "a whole file stays as it is"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filing_never_replaces_an_earlier_capture() {
        let dir =
            std::env::temp_dir().join(format!("cosmic-comp-file-into-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Captures")).unwrap();
        std::fs::write(dir.join("Captures/Shot.png"), b"old").unwrap();
        std::fs::write(dir.join("Shot.png"), b"new").unwrap();
        let to = file_into(&dir.join("Shot.png"), &dir.join("Captures")).unwrap();
        assert_eq!(to, dir.join("Captures/Shot (2).png"));
        assert_eq!(
            std::fs::read(dir.join("Captures/Shot.png")).unwrap(),
            b"old"
        );
        assert!(!dir.join("Shot.png").exists());
        assert_eq!(
            file_into(&to, &dir.join("Captures")).unwrap(),
            to,
            "saved files stay put"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn captures_are_named_by_kind_and_time() {
        assert_eq!(
            capture_stem(CaptureKind::Screenshot, &at(14, 25, 30)),
            "Screenshot 2026-09-10 at 14.25.30"
        );
        assert_eq!(
            capture_stem(CaptureKind::Recording, &at(9, 5, 0)),
            "Screen Recording 2026-09-10 at 09.05.00"
        );
    }
}
