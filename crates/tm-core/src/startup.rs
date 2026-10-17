//! Programs that start with the desktop session, from XDG autostart
//! (<https://specifications.freedesktop.org/autostart-spec/>). Needs no privileges: the
//! user's own `autostart` directory overrides the system ones, which are only ever read.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupEntry {
    /// The `.desktop` file name; the user's file of the same name overrides a system one.
    pub id: String,
    pub name: String,
    pub exec: String,
    pub enabled: bool,
    /// True when the winning file lives in the user's own autostart directory.
    pub user: bool,
}

#[derive(Debug, Clone)]
pub struct StartupDirs {
    pub user: PathBuf,
    pub system: Vec<PathBuf>,
    /// `XDG_CURRENT_DESKTOP`, split on `:`; filters `OnlyShowIn` / `NotShowIn`.
    pub desktops: Vec<String>,
}

impl StartupDirs {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let user_config = var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from(".config"));
        let system = var("XDG_CONFIG_DIRS")
            .unwrap_or_else(|| "/etc/xdg".into())
            .split(':')
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(d).join("autostart"))
            .collect();
        Self {
            user: user_config.join("autostart"),
            system,
            desktops: var("XDG_CURRENT_DESKTOP")
                .map(|d| d.split(':').map(str::to_string).collect())
                .unwrap_or_default(),
        }
    }
}

/// The `[Desktop Entry]` group of a `.desktop` file as key/value pairs. Localised keys such
/// as `Name[de]` are ignored.
fn desktop_entry(text: &str) -> HashMap<&str, &str> {
    let mut in_group = false;
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
        } else if in_group
            && !line.starts_with('#')
            && let Some((k, v)) = line.split_once('=')
            && !k.contains('[')
        {
            out.entry(k.trim()).or_insert(v.trim());
        }
    }
    out
}

fn is_true(v: Option<&&str>) -> bool {
    v.is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn list_has(v: Option<&&str>, desktops: &[String]) -> bool {
    v.is_some_and(|v| v.split(';').any(|d| desktops.iter().any(|c| c == d)))
}

fn parse_entry(id: &str, text: &str, user: bool, desktops: &[String]) -> Option<StartupEntry> {
    let kv = desktop_entry(text);
    if kv.get("Type").is_some_and(|t| *t != "Application") {
        return None;
    }
    // Entries meant for another desktop never run here, so they are not startup programs.
    if let Some(only) = kv.get("OnlyShowIn")
        && !list_has(Some(only), desktops)
    {
        return None;
    }
    if list_has(kv.get("NotShowIn"), desktops) {
        return None;
    }
    let exec = kv.get("Exec")?.to_string();
    let enabled = !is_true(kv.get("Hidden"))
        && !kv
            .get("X-GNOME-Autostart-enabled")
            .is_some_and(|v| v.eq_ignore_ascii_case("false"));
    Some(StartupEntry {
        id: id.to_string(),
        name: kv
            .get("Name")
            .map(|n| n.to_string())
            .unwrap_or_else(|| id.trim_end_matches(".desktop").to_string()),
        exec,
        enabled,
        user,
    })
}

/// Autostart entries, the user's overriding the system's by file name, sorted by name.
pub fn list_startup(dirs: &StartupDirs) -> Vec<StartupEntry> {
    let mut by_id: HashMap<String, StartupEntry> = HashMap::new();
    // Lowest precedence first, so later directories overwrite earlier ones.
    let ordered = dirs
        .system
        .iter()
        .rev()
        .map(|d| (d, false))
        .chain([(&dirs.user, true)]);
    // User files that cannot run on their own: a bare `Hidden=true` stub (the usual way other
    // tools disable a system entry) or something not meant for this desktop.
    let mut overridden: Vec<(String, bool)> = Vec::new();
    for (dir, user) in ordered {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for f in rd.flatten() {
            let id = f.file_name().to_string_lossy().into_owned();
            if !id.ends_with(".desktop") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(f.path()) else {
                continue;
            };
            match parse_entry(&id, &text, user, &dirs.desktops) {
                Some(e) => {
                    by_id.insert(id, e);
                }
                None if user => {
                    let hidden = is_true(desktop_entry(&text).get("Hidden"));
                    overridden.push((id, hidden));
                }
                None => {}
            }
        }
    }
    for (id, hidden) in overridden {
        match by_id.get_mut(&id) {
            Some(e) if hidden => (e.enabled, e.user) = (false, true),
            _ => {
                by_id.remove(&id);
            }
        }
    }
    let mut v: Vec<_> = by_id.into_values().collect();
    v.sort_by_cached_key(|e| (e.name.to_lowercase(), e.id.clone()));
    v
}

/// Sets `key=value` inside the `[Desktop Entry]` group, replacing an existing line or adding
/// one at the end of the group, and leaves everything else untouched.
fn set_key(text: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut in_group = false;
    let mut last_in_group = None;
    for (i, l) in lines.iter_mut().enumerate() {
        let t = l.trim();
        if t.starts_with('[') {
            in_group = t == "[Desktop Entry]";
        } else if in_group {
            if t.split_once('=').is_some_and(|(k, _)| k.trim() == key) {
                *l = format!("{key}={value}");
                return finish(lines);
            }
            if !t.is_empty() {
                last_in_group = Some(i);
            }
        }
    }
    match last_in_group {
        Some(i) => lines.insert(i + 1, format!("{key}={value}")),
        None => {
            lines.push("[Desktop Entry]".into());
            lines.push(format!("{key}={value}"));
        }
    }
    finish(lines)
}

fn finish(lines: Vec<String>) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// Enables or disables an entry by writing the user's override: the winning file is copied
/// into the user's autostart directory (if it is not already there) with `Hidden` set.
pub fn set_startup_enabled(dirs: &StartupDirs, id: &str, enabled: bool) -> io::Result<()> {
    if id.contains('/') || !id.ends_with(".desktop") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a .desktop file name",
        ));
    }
    let user_file = dirs.user.join(id);
    // Start from the user's file if it is a complete entry, else from the system one it overrides.
    let own = std::fs::read_to_string(&user_file)
        .ok()
        .filter(|t| desktop_entry(t).contains_key("Exec"));
    let text = match own {
        Some(t) => t,
        None => {
            let system = dirs
                .system
                .iter()
                .map(|d| d.join(id))
                .find(|p| p.exists())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such startup entry"))?;
            std::fs::read_to_string(system)?
        }
    };
    let text = set_key(&text, "Hidden", if enabled { "false" } else { "true" });
    let text = set_key(
        &text,
        "X-GNOME-Autostart-enabled",
        if enabled { "true" } else { "false" },
    );
    std::fs::create_dir_all(&dirs.user)?;
    std::fs::write(user_file, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tm-startup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(dir: &Path, file: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(file), body).unwrap();
    }

    fn dirs(root: &Path) -> StartupDirs {
        StartupDirs {
            user: root.join("user"),
            system: vec![root.join("sys1"), root.join("sys2")],
            desktops: vec!["GNOME".into(), "ubuntu".into()],
        }
    }

    const APP: &str = "[Desktop Entry]\nType=Application\nName=Syncer\nName[de]=Abgleich\nExec=/usr/bin/syncer --quiet\n";

    #[test]
    fn reads_name_exec_and_defaults() {
        let e = parse_entry("syncer.desktop", APP, false, &[]).unwrap();
        assert_eq!(e.name, "Syncer");
        assert_eq!(e.exec, "/usr/bin/syncer --quiet");
        assert!(e.enabled && !e.user);
        let bare = parse_entry("x.desktop", "[Desktop Entry]\nExec=x\n", true, &[]).unwrap();
        assert_eq!(bare.name, "x", "falls back to the file name");
        assert_eq!(
            parse_entry("n.desktop", "[Desktop Entry]\nName=N\n", false, &[]),
            None
        );
    }

    #[test]
    fn hidden_and_gnome_flags_disable() {
        for extra in ["Hidden=true", "X-GNOME-Autostart-enabled=false"] {
            let e = parse_entry("a.desktop", &format!("{APP}{extra}\n"), false, &[]).unwrap();
            assert!(!e.enabled, "{extra}");
        }
        let e = parse_entry("a.desktop", &format!("{APP}Hidden=false\n"), false, &[]).unwrap();
        assert!(e.enabled);
    }

    #[test]
    fn other_desktops_are_filtered_out() {
        let d = vec!["GNOME".to_string()];
        assert!(parse_entry("a", &format!("{APP}OnlyShowIn=KDE;\n"), false, &d).is_none());
        assert!(parse_entry("a", &format!("{APP}OnlyShowIn=KDE;GNOME;\n"), false, &d).is_some());
        assert!(parse_entry("a", &format!("{APP}NotShowIn=GNOME;\n"), false, &d).is_none());
        assert!(parse_entry("a", &format!("{APP}NotShowIn=KDE;\n"), false, &d).is_some());
    }

    #[test]
    fn user_files_override_system_ones_and_dirs_are_ordered() {
        let root = tmp("list");
        let d = dirs(&root);
        write(
            &d.system[0],
            "a.desktop",
            &APP.replace("Syncer", "A from sys1"),
        );
        write(
            &d.system[1],
            "a.desktop",
            &APP.replace("Syncer", "A from sys2"),
        );
        write(&d.system[1], "b.desktop", &APP.replace("Syncer", "B"));
        write(&d.system[1], "c.desktop", &APP.replace("Syncer", "C"));
        write(
            &d.user,
            "b.desktop",
            &format!("{APP}Hidden=true\n").replace("Syncer", "B mine"),
        );
        write(&d.user, "c.desktop", "[Desktop Entry]\nHidden=true\n");
        write(&d.user, "notes.txt", "ignored");
        let list = list_startup(&d);
        let got: Vec<_> = list
            .iter()
            .map(|e| (e.name.as_str(), e.enabled, e.user))
            .collect();
        assert_eq!(
            got,
            [
                ("A from sys1", true, false),
                ("B mine", false, true),
                ("C", false, true)
            ],
            "a bare Hidden stub disables the system entry but keeps it listed"
        );
        set_startup_enabled(&d, "c.desktop", true).unwrap();
        let c = list_startup(&d)
            .into_iter()
            .find(|e| e.id == "c.desktop")
            .unwrap();
        assert!(
            c.enabled,
            "re-enabling completes the stub from the system file"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn set_key_replaces_or_appends_inside_the_group() {
        let t = "[Desktop Entry]\nName=N\nHidden=false\n\n[Desktop Action x]\nName=X\n";
        assert_eq!(
            set_key(t, "Hidden", "true"),
            "[Desktop Entry]\nName=N\nHidden=true\n\n[Desktop Action x]\nName=X\n"
        );
        assert_eq!(
            set_key(t, "Foo", "1"),
            "[Desktop Entry]\nName=N\nHidden=false\nFoo=1\n\n[Desktop Action x]\nName=X\n"
        );
        assert_eq!(
            set_key("", "Hidden", "true"),
            "[Desktop Entry]\nHidden=true\n"
        );
    }

    #[test]
    fn disable_then_enable_round_trips_through_a_user_copy() {
        let root = tmp("toggle");
        let d = dirs(&root);
        write(&d.system[0], "s.desktop", APP);

        set_startup_enabled(&d, "s.desktop", false).unwrap();
        let e = &list_startup(&d)[0];
        assert!(!e.enabled && e.user, "disabled via a user override");
        assert_eq!(
            std::fs::read_to_string(d.system[0].join("s.desktop")).unwrap(),
            APP,
            "system file untouched"
        );

        set_startup_enabled(&d, "s.desktop", true).unwrap();
        let e = &list_startup(&d)[0];
        assert!(e.enabled);
        assert_eq!(e.exec, "/usr/bin/syncer --quiet");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_paths_and_unknown_entries() {
        let root = tmp("bad");
        let d = dirs(&root);
        assert!(set_startup_enabled(&d, "../evil.desktop", false).is_err());
        assert!(set_startup_enabled(&d, "x.txt", false).is_err());
        assert_eq!(
            set_startup_enabled(&d, "missing.desktop", false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
