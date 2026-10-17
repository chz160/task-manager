//! System services from systemd's D-Bus API. Reading the unit list needs no privileges.

use zbus::blocking::Connection;
use zbus::zvariant::OwnedObjectPath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    /// Unit name without the `.service` suffix, e.g. `cron`.
    pub name: String,
    pub description: String,
    pub active_state: String,
    pub sub_state: String,
}

impl ServiceInfo {
    /// Windows-style status word for the Services table.
    pub fn status(&self) -> &str {
        match (self.active_state.as_str(), self.sub_state.as_str()) {
            ("active", "running") => "Running",
            ("active", "exited") => "Exited",
            ("active", _) | ("reloading", _) => "Running",
            ("activating", _) => "Starting",
            ("deactivating", _) => "Stopping",
            ("failed", _) => "Failed",
            ("inactive", _) => "Stopped",
            _ => "Unknown",
        }
    }
}

/// One row of `org.freedesktop.systemd1.Manager.ListUnits`:
/// name, description, load state, active state, sub state, followed unit, object path,
/// queued job id, job type, job path.
type Unit = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

fn services_from_units(units: Vec<Unit>) -> Vec<ServiceInfo> {
    units
        .into_iter()
        .filter_map(|(id, description, load, active, sub, ..)| {
            // Units that were only referenced by another unit have no file behind them.
            if load == "not-found" {
                return None;
            }
            Some(ServiceInfo {
                name: id.strip_suffix(".service")?.to_string(),
                description,
                active_state: active,
                sub_state: sub,
            })
        })
        .collect()
}

/// Lists the loaded system services, sorted by name.
pub fn list_services() -> Result<Vec<ServiceInfo>, String> {
    let conn = Connection::system().map_err(|e| format!("Could not reach the system bus: {e}"))?;
    list_services_on(&conn)
}

fn list_services_on(conn: &Connection) -> Result<Vec<ServiceInfo>, String> {
    let reply = conn
        .call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "ListUnits",
            &(),
        )
        .map_err(|e| format!("Could not ask systemd for its services: {e}"))?;
    let units: Vec<Unit> = reply
        .body()
        .deserialize()
        .map_err(|e| format!("Unexpected reply from systemd: {e}"))?;
    let mut services = services_from_units(units);
    services.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(id: &str, load: &str, active: &str, sub: &str) -> Unit {
        (
            id.into(),
            format!("{id} description"),
            load.into(),
            active.into(),
            sub.into(),
            String::new(),
            OwnedObjectPath::try_from("/").unwrap(),
            0,
            String::new(),
            OwnedObjectPath::try_from("/").unwrap(),
        )
    }

    #[test]
    fn keeps_only_loaded_services() {
        let s = services_from_units(vec![
            unit("cron.service", "loaded", "active", "running"),
            unit("sshd.socket", "loaded", "active", "listening"),
            unit("ghost.service", "not-found", "inactive", "dead"),
            unit("fstrim.service", "loaded", "inactive", "dead"),
        ]);
        let names: Vec<_> = s.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["cron", "fstrim"]);
        assert_eq!(s[0].description, "cron.service description");
    }

    #[test]
    fn status_words() {
        let st = |a: &str, b: &str| {
            ServiceInfo {
                name: "x".into(),
                description: String::new(),
                active_state: a.into(),
                sub_state: b.into(),
            }
            .status()
            .to_string()
        };
        assert_eq!(st("active", "running"), "Running");
        assert_eq!(st("active", "exited"), "Exited");
        assert_eq!(st("inactive", "dead"), "Stopped");
        assert_eq!(st("failed", "failed"), "Failed");
        assert_eq!(st("activating", "start"), "Starting");
        assert_eq!(st("weird", "x"), "Unknown");
    }

    /// A stand-in for systemd's Manager object, so the wire format is tested end to end.
    struct FakeManager;

    #[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
    impl FakeManager {
        fn list_units(&self) -> Vec<Unit> {
            vec![
                unit("b.service", "loaded", "active", "running"),
                unit("a.service", "loaded", "failed", "failed"),
                unit("x.timer", "loaded", "active", "waiting"),
            ]
        }
    }

    /// Needs a session bus (`dbus-run-session cargo test`); skipped without one.
    #[test]
    fn reads_the_unit_list_over_d_bus() {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            eprintln!("skipped: no session bus");
            return;
        }
        let _server = zbus::blocking::connection::Builder::session()
            .unwrap()
            .name("org.freedesktop.systemd1")
            .unwrap()
            .serve_at("/org/freedesktop/systemd1", FakeManager)
            .unwrap()
            .build()
            .unwrap();
        let client = Connection::session().unwrap();
        let s = list_services_on(&client).unwrap();
        let got: Vec<_> = s.iter().map(|s| (s.name.as_str(), s.status())).collect();
        assert_eq!(got, [("a", "Failed"), ("b", "Running")]);
    }
}
