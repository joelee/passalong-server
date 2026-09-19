//! The files `service install` writes, as text. No I/O here.

use std::path::Path;

/// The service's user and group.
pub const USER: &str = "passalong-server";

/// The unit's name.
pub const UNIT: &str = "passalong-server.service";

/// The line by which `service install` knows a unit file as its own. A file
/// at the unit's path without it was put there by someone else, and is
/// neither overwritten nor removed.
pub const MARKER: &str = "# Written by `passalong-server service install`.";

const TEMPLATE: &str = include_str!("unit.service.in");

/// The unit, for a binary at `binary`.
pub fn render_unit(binary: &Path) -> String {
    TEMPLATE.replace("{{binary}}", &binary.display().to_string())
}

/// The `sysusers.d` file that makes the service's user: a system user with
/// the data directory as its home, who cannot log in.
pub fn render_sysusers(data_dir: &Path) -> String {
    format!(
        "# The user `passalong-server serve` runs as.\n{MARKER}\nu {USER} - \"passalong-server\" {} /usr/sbin/nologin\n",
        data_dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(file: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/service")
            .join(file);
        std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
    }

    #[test]
    fn the_repositorys_unit_is_what_install_writes() {
        let rendered = render_unit(Path::new("/usr/local/bin/passalong-server"));
        assert_eq!(
            rendered,
            repository("passalong-server.service"),
            "docs/service/passalong-server.service is not the rendered template; \
             render it again from src/service/unit.service.in"
        );
        assert_eq!(
            render_sysusers(Path::new("/var/lib/passalong-server")),
            repository("passalong-server.sysusers")
        );
    }

    #[test]
    fn the_unit_says_what_the_server_needs_of_it() {
        let unit = render_unit(Path::new("/opt/pass/bin/passalong-server"));
        for line in [
            "ExecStart=/opt/pass/bin/passalong-server serve",
            "User=passalong-server",
            // Longer than the 30 seconds `serve` drains for.
            "TimeoutStopSec=45",
            "StateDirectory=passalong-server",
            "ProtectSystem=strict",
            "NoNewPrivileges=true",
            MARKER,
        ] {
            assert!(unit.lines().any(|have| have == line), "no line {line:?}");
        }
        assert!(!unit.contains("{{"), "a placeholder was left");
        assert!(!unit.contains("DRAFT"));
        assert!(render_sysusers(Path::new("/var/lib/passalong-server")).contains(MARKER));
    }
}
