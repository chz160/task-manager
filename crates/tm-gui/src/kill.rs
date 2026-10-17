//! End Process. Signals directly when allowed; for processes owned by other
//! users, asks polkit (via `pkexec`) to run `kill` as root. The GUI itself
//! never runs as root.

use std::io;
use std::process::Command;

const KILL_BINARIES: [&str; 2] = ["/usr/bin/kill", "/bin/kill"];

pub fn end_processes(pids: &[i32]) -> Result<(), String> {
    end_processes_via(pids, send_sigterm, elevated_kill)
}

/// Signals every PID directly. The ones this user may not signal go to a single elevated
/// call, so a group of other users' processes asks for authorisation once.
fn end_processes_via(
    pids: &[i32],
    signal: impl Fn(i32) -> io::Result<()>,
    elevate: impl FnOnce(&[i32]) -> Result<(), String>,
) -> Result<(), String> {
    let mut denied = Vec::new();
    let mut failures = Vec::new();
    for &pid in pids {
        match signal(pid) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => denied.push(pid),
            // Exiting on its own while the dialog was open is the outcome the user wanted.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if pids.len() == 1 {
                    return Err(format!("Process {pid} has already exited."));
                }
            }
            Err(e) => failures.push(format!("Could not end process {pid}: {e}")),
        }
    }
    if !denied.is_empty() {
        elevate(&denied)?;
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
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

fn elevated_kill(pids: &[i32]) -> Result<(), String> {
    let kill = KILL_BINARIES
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .ok_or("Could not find the kill program.")?;
    let out = Command::new("pkexec")
        .arg(kill)
        .args(["-TERM", "--"])
        .args(pids.iter().map(i32::to_string))
        .output()
        .map_err(|e| format!("Could not ask for permission (is polkit installed?): {e}"))?;
    match out.status.code() {
        Some(0) => Ok(()),
        // pkexec: 126 = dialog dismissed, 127 = not authorized.
        Some(126) => Err("Permission was not granted, so nothing was ended.".into()),
        Some(127) => Err("You are not authorized to end this process.".into()),
        _ => Err(format!(
            "Could not end the process: {}",
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
    fn elevates_only_the_pids_that_were_denied_and_only_once() {
        let calls = std::cell::RefCell::new(Vec::new());
        let r = end_processes_via(
            &[1, 2, 3],
            |p| {
                if p == 2 {
                    err(io::ErrorKind::PermissionDenied)
                } else {
                    Ok(())
                }
            },
            |denied| {
                calls.borrow_mut().push(denied.to_vec());
                Ok(())
            },
        );
        assert_eq!(r, Ok(()));
        assert_eq!(*calls.borrow(), vec![vec![2]]);

        let r = end_processes_via(&[7], |_| Ok(()), |_| panic!("must not elevate"));
        assert_eq!(r, Ok(()));
    }

    #[test]
    fn a_denied_group_asks_once() {
        let n = std::cell::Cell::new(0);
        let _ = end_processes_via(
            &[4, 5, 6],
            |_| err(io::ErrorKind::PermissionDenied),
            |d| {
                n.set(n.get() + 1);
                assert_eq!(d, [4, 5, 6]);
                Ok(())
            },
        );
        assert_eq!(n.get(), 1);
    }

    #[test]
    fn exited_processes_are_reported_only_for_a_single_target() {
        let r = end_processes_via(&[7], |_| err(io::ErrorKind::NotFound), |_| panic!("no"));
        assert!(r.unwrap_err().contains("already exited"));
        let r = end_processes_via(&[7, 8], |_| err(io::ErrorKind::NotFound), |_| panic!("no"));
        assert_eq!(r, Ok(()));
    }

    #[test]
    fn elevation_failure_is_reported() {
        let r = end_processes_via(
            &[7],
            |_| err(io::ErrorKind::PermissionDenied),
            |_| Err("nope".into()),
        );
        assert_eq!(r, Err("nope".into()));
    }

    #[test]
    fn terminates_real_children() {
        let mut a = Command::new("sleep").arg("60").spawn().unwrap();
        let mut b = Command::new("sleep").arg("60").spawn().unwrap();
        end_processes(&[a.id() as i32, b.id() as i32]).unwrap();
        assert!(!a.wait().unwrap().success());
        assert!(!b.wait().unwrap().success());
    }
}
