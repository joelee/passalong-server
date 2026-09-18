//! Who may run the commands: the data directory's owner, and nobody else,
//! root included. The server and these commands share one database; a
//! database or journal file created by root would lock the server out, and
//! a server that cannot read its keys refuses every request.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// The effective user of this process. `/proc/self` is a link to
/// `/proc/<pid>`, which belongs to that user. The link must be followed:
/// the link itself belongs to root, whoever asks.
pub fn current_uid() -> Result<u32, String> {
    std::fs::metadata("/proc/self")
        .map(|meta| meta.uid())
        .map_err(|err| format!("cannot tell who is running this: /proc/self: {err}"))
}

/// Refuses unless `me` owns the directory.
pub fn same_owner(me: u32, owner: u32, dir: &Path, command: &str) -> Result<(), String> {
    if me == owner {
        return Ok(());
    }
    Err(format!(
        "{} belongs to user {owner}, and this is user {me}. The server and this command share one database; files created by another user would lock the server out. Run it as the owner:\n  sudo -u '#{owner}' {command}",
        dir.display()
    ))
}

/// Refuses unless this process's user owns `dir`.
pub fn check(dir: &Path, command: &str) -> Result<(), String> {
    let owner = std::fs::metadata(dir)
        .map_err(|err| {
            format!(
                "{}: {err}. `passalong-server init` creates it",
                dir.display()
            )
        })?
        .uid();
    same_owner(current_uid()?, owner, dir, command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_may_and_nobody_else_not_even_root() {
        let dir = Path::new("/var/lib/passalong-server");
        assert!(same_owner(10001, 10001, dir, "passalong-server key list").is_ok());
        for stranger in [0, 1000] {
            let said = same_owner(stranger, 10001, dir, "passalong-server key list").unwrap_err();
            assert!(
                said.contains("sudo -u '#10001' passalong-server key list"),
                "{said}"
            );
            assert!(said.contains("/var/lib/passalong-server"), "{said}");
        }
    }

    #[test]
    fn the_link_is_followed_so_the_answer_is_this_user_and_not_root() {
        // A file this process creates belongs to its effective user. Were
        // the link read instead of followed, the answer would be 0 for
        // everyone, and the check would refuse all or, as root, accept root.
        let dir = tempfile::tempdir().unwrap();
        let mine = std::fs::metadata(dir.path()).unwrap().uid();
        assert_eq!(current_uid().unwrap(), mine);
        assert!(check(dir.path(), "x").is_ok());
        assert!(
            check(&dir.path().join("missing"), "x")
                .unwrap_err()
                .contains("init")
        );
    }
}
