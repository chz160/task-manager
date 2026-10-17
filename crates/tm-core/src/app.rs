//! Names the application a process belongs to from its cgroup path, the way a desktop
//! session manager launches things (systemd `app-*.scope`, flatpak, snap, services).

fn unescape(s: &str) -> String {
    // systemd escapes '-' and other bytes as \xNN inside unit names.
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("\\x") {
        out.push_str(&rest[..i]);
        let hex = rest.get(i + 2..i + 4);
        match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
            Some(b) => {
                out.push(b as char);
                rest = &rest[i + 4..];
            }
            None => {
                out.push_str("\\x");
                rest = &rest[i + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn strip_trailing_number(s: &str) -> &str {
    match s.rsplit_once('-') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => s,
    }
}

/// "org.mozilla.firefox" -> "firefox".
fn short_id(s: &str) -> &str {
    if s.matches('.').count() >= 2 {
        s.rsplit('.').next().filter(|l| !l.is_empty()).unwrap_or(s)
    } else {
        s
    }
}

const LAUNCHERS: [&str; 8] = [
    "gnome", "kde", "flatpak", "snap", "xdg", "cinnamon", "mate", "xfce",
];

/// The application or service a cgroup path stands for, or `None` for login sessions,
/// slices and anything else that says nothing about the program (callers then fall back
/// to the process name).
pub fn app_from_cgroup(path: &str) -> Option<String> {
    let last = path.trim_end_matches('/').rsplit('/').next()?;
    let last = unescape(last);
    let name = if let Some(rest) = last.strip_prefix("app-") {
        let unit = rest
            .strip_suffix(".scope")
            .or_else(|| rest.strip_suffix(".service"))?;
        let unit = unit.split('@').next().unwrap_or(unit);
        let unit = strip_trailing_number(unit);
        let unit = LAUNCHERS
            .iter()
            .find_map(|l| unit.strip_prefix(l).and_then(|r| r.strip_prefix('-')))
            .unwrap_or(unit);
        short_id(unit).to_string()
    } else if let Some(rest) = last.strip_prefix("snap.") {
        // snap.<snap>.<app>-<uuid>.scope
        let rest = rest.strip_suffix(".scope")?;
        rest.split('.').next()?.to_string()
    } else {
        let svc = last.strip_suffix(".service")?;
        svc.split('@').next().unwrap_or(svc).to_string()
    };
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(p: &str) -> Option<String> {
        app_from_cgroup(p)
    }

    #[test]
    fn desktop_app_scopes() {
        let base = "/user.slice/user-1000.slice/user@1000.service/app.slice/";
        assert_eq!(
            app(&format!("{base}app-gnome-firefox-4242.scope")).as_deref(),
            Some("firefox")
        );
        assert_eq!(
            app(&format!("{base}app-flatpak-org.mozilla.firefox-99.scope")).as_deref(),
            Some("firefox")
        );
        assert_eq!(
            app(&format!("{base}app-gnome-org.gnome.Terminal-12.scope")).as_deref(),
            Some("Terminal")
        );
        assert_eq!(
            app(&format!("{base}app-firefox@a1b2c3.service")).as_deref(),
            Some("firefox")
        );
        assert_eq!(
            app(&format!("{base}app-gnome-my\\x2dtool-7.scope")).as_deref(),
            Some("my-tool")
        );
    }

    #[test]
    fn snap_and_services() {
        assert_eq!(
            app("/user.slice/snap.firefox.firefox-1a2b-3c.scope").as_deref(),
            Some("firefox")
        );
        assert_eq!(app("/system.slice/cron.service").as_deref(), Some("cron"));
        assert_eq!(
            app("/user.slice/user-1000.slice/user@1000.service/session.slice/pipewire.service")
                .as_deref(),
            Some("pipewire")
        );
        assert_eq!(
            app("/system.slice/getty@tty1.service").as_deref(),
            Some("getty")
        );
    }

    #[test]
    fn sessions_and_slices_have_no_app() {
        assert_eq!(app("/user.slice/user-1000.slice/session-3.scope"), None);
        assert_eq!(app("/init.scope"), None);
        assert_eq!(app("/"), None);
        assert_eq!(app(""), None);
        assert_eq!(app("/user.slice/user-1000.slice"), None);
        assert_eq!(app("/system.slice/docker-abc.scope"), None);
    }
}
