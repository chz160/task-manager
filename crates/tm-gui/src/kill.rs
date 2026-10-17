//! End Process. Signals directly when allowed; for processes owned by other
//! users, asks polkit (via `pkexec`) to run `kill` as root. The GUI itself
//! never runs as root.

use std::io;
use std::process::Command;

const KILL_BINARIES: [&str; 2] = ["/usr/bin/kill", "/bin/kill"];

pub fn end_process(pid: i32) -> Result<(), String> {
    end_process_via(pid, send_sigterm, elevated_kill)
}

fn end_process_via(
    pid: i32,
    signal: impl FnOnce(i32) -> io::Result<()>,
    elevate: impl FnOnce(i32) -> Result<(), String>,
) -> Result<(), String> {
    match signal(pid) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => elevate(pid),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(format!("Process {pid} has already exited."))
        }
        Err(e) => Err(format!("Could not end process {pid}: {e}")),
    }
}

fn send_sigterm(pid: i32) -> io::Result<()> {
    // SAFETY: kill(2) takes plain integers and has no memory-safety preconditions.
    if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn elevated_kill(pid: i32) -> Result<(), String> {
    let kill = KILL_BINARIES
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .ok_or("Could not find the kill program.")?;
    let out = Command::new("pkexec")
        .args([kill, "-TERM", "--", &pid.to_string()])
        .output()
        .map_err(|e| format!("Could not ask for permission (is polkit installed?): {e}"))?;
    match out.status.code() {
        Some(0) => Ok(()),
        // pkexec: 126 = dialog dismissed, 127 = not authorized.
        Some(126) => Err("Permission was not granted, so the process was not ended.".into()),
        Some(127) => Err("You are not authorized to end this process.".into()),
        _ => Err(format!(
            "Could not end process {pid}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn err(kind: io::ErrorKind) -> io::Result<()> {
        Err(io::Error::from(kind))
    }

    #[test]
    fn elevates_only_on_permission_denied() {
        let r = end_process_via(
            7,
            |_| err(io::ErrorKind::PermissionDenied),
            |p| {
                assert_eq!(p, 7);
                Ok(())
            },
        );
        assert_eq!(r, Ok(()));

        let r = end_process_via(7, |_| Ok(()), |_| panic!("must not elevate"));
        assert_eq!(r, Ok(()));

        let r = end_process_via(
            7,
            |_| err(io::ErrorKind::NotFound),
            |_| panic!("must not elevate"),
        );
        assert!(r.unwrap_err().contains("already exited"));
    }

    #[test]
    fn elevation_failure_is_reported() {
        let r = end_process_via(
            7,
            |_| err(io::ErrorKind::PermissionDenied),
            |_| Err("nope".into()),
        );
        assert_eq!(r, Err("nope".into()));
    }

    #[test]
    fn terminates_a_real_child() {
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        end_process(child.id() as i32).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success());
    }
}
