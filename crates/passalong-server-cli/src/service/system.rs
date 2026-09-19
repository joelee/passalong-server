//! The host, as `service install` needs it. Everything that touches the
//! machine goes through [`System`], so the commands are tested with a fake
//! and the real one stays small: `std`, and the two programs systemd brings.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use passalong_server_core::clock::{Clock, SystemClock};

/// A user id and a group id.
pub type Ids = (u32, u32);

pub trait System {
    fn is_root(&self) -> bool;
    /// Where this binary runs from.
    fn running_binary(&self) -> Result<PathBuf, String>;
    /// A text file's content; `None` when there is no such file.
    fn read(&self, path: &Path) -> Option<String>;
    fn exists(&self, path: &Path) -> bool;
    /// Whether both are files with the same bytes.
    fn same_content(&self, one: &Path, other: &Path) -> bool;
    /// The ids of a user of this host.
    fn user_ids(&self, name: &str) -> Option<Ids>;
    /// Creates `path` and what is missing above it, and sets its own mode
    /// and owner, also when it was there.
    fn make_dir(&self, path: &Path, mode: u32, owner: Option<Ids>) -> Result<(), String>;
    /// Writes a file whole: never half of it, and never readable by others
    /// before it has its mode.
    fn write_file(
        &self,
        path: &Path,
        content: &[u8],
        mode: u32,
        owner: Option<Ids>,
    ) -> Result<(), String>;
    /// Copies a binary into place, over one that may be running.
    fn install_binary(&self, from: &Path, to: &Path) -> Result<(), String>;
    fn remove_file(&self, path: &Path) -> Result<(), String>;
    fn run(&self, program: &str, args: &[String]) -> Result<(), String>;
    fn now(&self) -> u64;
}

/// This host.
#[derive(Debug, Clone)]
pub struct Host {
    /// `/etc/passwd`, or a test's stand-in.
    pub passwd: PathBuf,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            passwd: "/etc/passwd".into(),
        }
    }
}

fn at<T>(path: &Path, result: std::io::Result<T>) -> Result<T, String> {
    result.map_err(|err| format!("{}: {err}", path.display()))
}

fn beside(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".new");
    path.with_file_name(name)
}

/// The ids of `name` in the text of a `passwd` file.
pub fn ids_in(passwd: &str, name: &str) -> Option<Ids> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? != name {
            return None;
        }
        let uid = fields.nth(1)?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        Some((uid, gid))
    })
}

/// Written aside, given its mode and owner, and only then moved into place.
fn place(
    path: &Path,
    mode: u32,
    owner: Option<Ids>,
    fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), String> {
    let aside = beside(path);
    let _ = std::fs::remove_file(&aside);
    let written = (|| {
        // A minimal host has no `/etc/sysusers.d`. What is missing above the
        // file is made; what is there is left exactly as it is.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&aside)?;
        fill(&mut file)?;
        file.sync_all()?;
        std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(mode))?;
        if let Some((uid, gid)) = owner {
            std::os::unix::fs::chown(&aside, Some(uid), Some(gid))?;
        }
        std::fs::rename(&aside, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&aside);
    }
    at(path, written)
}

impl System for Host {
    fn is_root(&self) -> bool {
        crate::owner::current_uid().is_ok_and(|uid| uid == 0)
    }

    fn running_binary(&self) -> Result<PathBuf, String> {
        std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|err| format!("cannot tell where this binary is: {err}"))
    }

    fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn same_content(&self, one: &Path, other: &Path) -> bool {
        match (std::fs::read(one), std::fs::read(other)) {
            (Ok(one), Ok(other)) => one == other,
            _ => false,
        }
    }

    fn user_ids(&self, name: &str) -> Option<Ids> {
        ids_in(&self.read(&self.passwd)?, name)
    }

    fn make_dir(&self, path: &Path, mode: u32, owner: Option<Ids>) -> Result<(), String> {
        at(
            path,
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(mode)
                .create(path)
                .and_then(|()| {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                })
                .and_then(|()| match owner {
                    Some((uid, gid)) => std::os::unix::fs::chown(path, Some(uid), Some(gid)),
                    None => Ok(()),
                }),
        )
    }

    fn write_file(
        &self,
        path: &Path,
        content: &[u8],
        mode: u32,
        owner: Option<Ids>,
    ) -> Result<(), String> {
        place(path, mode, owner, |file| file.write_all(content))
    }

    fn install_binary(&self, from: &Path, to: &Path) -> Result<(), String> {
        let mut source = at(from, std::fs::File::open(from))?;
        // Moved into place, not written into it: a binary that runs cannot
        // be written to, and can be replaced.
        place(to, 0o755, None, |file| {
            std::io::copy(&mut source, file).map(|_| ())
        })
    }

    fn remove_file(&self, path: &Path) -> Result<(), String> {
        match std::fs::remove_file(path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => at(path, Err(err)),
            _ => Ok(()),
        }
    }

    fn run(&self, program: &str, args: &[String]) -> Result<(), String> {
        let shown = format!("{program} {}", args.join(" "));
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|err| format!("`{shown}`: {err}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "`{shown}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn now(&self) -> u64 {
        SystemClock.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn a_users_ids_are_read_from_passwd() {
        let passwd = "root:x:0:0::/root:/bin/bash\npassalong-server:x:987:985:passalong-server:/var/lib/passalong-server:/usr/sbin/nologin\nbroken:x:abc:1::/:/bin/sh\n";
        assert_eq!(ids_in(passwd, "passalong-server"), Some((987, 985)));
        assert_eq!(ids_in(passwd, "root"), Some((0, 0)));
        assert_eq!(ids_in(passwd, "passalong"), None);
        assert_eq!(ids_in(passwd, "broken"), None);
        let dir = tempfile::tempdir().unwrap();
        let host = Host {
            passwd: dir.path().join("passwd"),
        };
        assert_eq!(host.user_ids("root"), None);
        std::fs::write(&host.passwd, passwd).unwrap();
        assert_eq!(host.user_ids("passalong-server"), Some((987, 985)));
    }

    #[test]
    fn files_and_directories_get_their_mode_and_owner_and_no_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::default();
        let me = {
            let meta = std::fs::metadata(dir.path()).unwrap();
            Some((meta.uid(), meta.gid()))
        };
        let private = dir.path().join("etc/passalong-server/tls");
        host.make_dir(&private, 0o700, me).unwrap();
        assert_eq!(mode(&private), 0o700);
        // A directory that is there is brought to the same state.
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755)).unwrap();
        host.make_dir(&private, 0o700, None).unwrap();
        assert_eq!(mode(&private), 0o700);

        let key = private.join("key.pem");
        host.write_file(&key, b"first", 0o600, me).unwrap();
        host.write_file(&key, b"second", 0o600, None).unwrap();
        assert_eq!(host.read(&key).as_deref(), Some("second"));
        assert_eq!(mode(&key), 0o600);
        let unit = private.join("unit");
        host.write_file(&unit, b"[Unit]", 0o644, None).unwrap();
        assert_eq!(mode(&unit), 0o644);
        assert!(host.exists(&unit) && !host.exists(&beside(&unit)));

        // Into a directory that is not there, as `/etc/sysusers.d` is not on
        // a minimal host: it is made, and one that is there keeps its mode.
        let deeper = dir.path().join("etc/sysusers.d/passalong-server.conf");
        host.write_file(&deeper, b"u", 0o644, None).unwrap();
        assert_eq!(host.read(&deeper).as_deref(), Some("u"));
        assert_eq!(mode(&private), 0o700);
        // Where a file is in the way of the directory: refused, and said.
        let said = host
            .write_file(&key.join("below"), b"x", 0o600, None)
            .unwrap_err();
        assert!(said.contains("key.pem/below"), "{said}");
        // To an owner this user may not give files to: refused, nothing left.
        if !host.is_root() {
            let theirs = private.join("theirs");
            assert!(host.write_file(&theirs, b"x", 0o600, Some((0, 0))).is_err());
            assert!(!host.exists(&theirs) && !host.exists(&beside(&theirs)));
        }

        host.remove_file(&unit).unwrap();
        host.remove_file(&unit).unwrap();
        assert!(!host.exists(&unit));
        assert_eq!(host.read(&unit), None);
    }

    #[test]
    fn a_binary_is_moved_into_place_executable() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::default();
        let running = host.running_binary().unwrap();
        assert!(running.is_absolute() && running.is_file());
        let to = dir.path().join("passalong-server");
        std::fs::write(&to, "an older one").unwrap();
        assert!(!host.same_content(&running, &to));
        host.install_binary(&running, &to).unwrap();
        assert_eq!(mode(&to), 0o755);
        assert_eq!(
            std::fs::metadata(&to).unwrap().len(),
            std::fs::metadata(&running).unwrap().len()
        );
        assert!(host.same_content(&running, &to));
        assert!(host.install_binary(&dir.path().join("none"), &to).is_err());
        assert!(!host.same_content(&running, &dir.path().join("none")));
    }

    #[test]
    fn a_program_that_fails_says_why() {
        let host = Host::default();
        host.run("true", &[]).unwrap();
        let said = host
            .run(
                "sh",
                &["-c".to_owned(), "echo no such unit >&2; exit 3".to_owned()],
            )
            .unwrap_err();
        assert!(
            said.contains("no such unit") && said.contains("sh -c"),
            "{said}"
        );
        let said = host.run("no-such-program-here", &[]).unwrap_err();
        assert!(said.contains("no-such-program-here"), "{said}");
        assert!(host.now() > 1_700_000_000);
    }
}
