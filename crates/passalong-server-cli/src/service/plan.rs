//! What `service install` and `service remove` will do, as a list. Deciding
//! is kept apart from doing, so that every decision is tested without root,
//! without systemd, and before anything on a host is touched.

use std::path::PathBuf;

use super::unit::{MARKER, UNIT, USER, render_sysusers, render_unit};

/// Where everything goes on a systemd host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub binary: PathBuf,
    pub unit: PathBuf,
    pub sysusers: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            binary: "/usr/local/bin/passalong-server".into(),
            unit: PathBuf::from("/etc/systemd/system").join(UNIT),
            sysusers: "/etc/sysusers.d/passalong-server.conf".into(),
            config_dir: "/etc/passalong-server".into(),
            data_dir: "/var/lib/passalong-server".into(),
        }
    }
}

impl Layout {
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}

/// Who a file or directory is given to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Root,
    /// The service's user, looked up when the action runs: an earlier
    /// action may be what creates it.
    Service,
}

/// One thing done to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Copies the running binary to its place, replacing an older one.
    InstallBinary {
        from: PathBuf,
        to: PathBuf,
    },
    /// Creates a directory, or sets the mode and owner of one that exists.
    MakeDir {
        path: PathBuf,
        mode: u32,
        owner: Owner,
    },
    WriteFile {
        path: PathBuf,
        content: String,
        mode: u32,
        owner: Owner,
    },
    /// Makes a self-signed pair for `names` and prints its pin.
    MakePair {
        cert_file: PathBuf,
        key_file: PathBuf,
        names: Vec<String>,
    },
    RemoveFile {
        path: PathBuf,
    },
    Run {
        program: &'static str,
        args: Vec<String>,
    },
}

fn run(program: &'static str, args: &[&str]) -> Action {
    Action::Run {
        program,
        args: args.iter().map(ToString::to_string).collect(),
    }
}

/// A file `service install` writes, as it is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    Absent,
    /// There, and exactly what would be written.
    Same,
    /// There, written by an earlier `service install`, and different.
    Ours,
    /// There, and not written by `service install`.
    Foreign,
}

impl Existing {
    /// `found` is the file's content, if there is a file.
    pub fn of(found: Option<&str>, wanted: &str) -> Self {
        match found {
            None => Self::Absent,
            Some(found) if found == wanted => Self::Same,
            Some(found) if found.lines().any(|line| line == MARKER) => Self::Ours,
            Some(_) => Self::Foreign,
        }
    }
}

/// What was found on the host before anything was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Where this binary runs from.
    pub running: PathBuf,
    /// Whether the binary in its place is, byte for byte, this one.
    pub binary_installed: bool,
    pub unit: Existing,
    pub sysusers: Existing,
    pub config_exists: bool,
    /// Whether the configuration, the one there or the one to be written,
    /// has `listen.mode = "tls"`, and where it expects the pair.
    pub tls: Option<(PathBuf, PathBuf)>,
    pub cert_exists: bool,
    pub key_exists: bool,
}

/// What `service install` was asked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wanted {
    /// Host names and addresses for a self-signed pair; empty for none.
    pub names: Vec<String>,
    /// The configuration to write if there is none.
    pub initial_config: String,
}

/// The plan, and what to tell the operator after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// `None` when the unit is started; otherwise why it is not.
    pub not_started: Option<String>,
}

fn foreign(path: &std::path::Path) -> String {
    format!(
        "{} exists and was not written by `passalong-server service install`. It is left as it is; move it away if this command is to write one",
        path.display()
    )
}

/// # Errors
///
/// When something is in the way that the command will not overwrite.
pub fn plan_install(layout: &Layout, found: &Found, wanted: &Wanted) -> Result<Plan, String> {
    for (existing, path) in [
        (found.unit, &layout.unit),
        (found.sysusers, &layout.sysusers),
    ] {
        if existing == Existing::Foreign {
            return Err(foreign(path));
        }
    }
    let pair = found.tls.as_ref();
    if let Some((cert_file, key_file)) = pair
        && found.cert_exists != found.key_exists
    {
        return Err(format!(
            "of the pair {} and {}, one is there and the other is not. Nothing is written over half a pair: complete it, or remove it",
            cert_file.display(),
            key_file.display()
        ));
    }

    let mut actions = Vec::new();
    let mut changed = false;
    if found.running != layout.binary && !found.binary_installed {
        actions.push(Action::InstallBinary {
            from: found.running.clone(),
            to: layout.binary.clone(),
        });
        changed = true;
    }
    if found.sysusers != Existing::Same {
        actions.push(Action::WriteFile {
            path: layout.sysusers.clone(),
            content: render_sysusers(&layout.data_dir),
            mode: 0o644,
            owner: Owner::Root,
        });
        actions.push(run(
            "systemd-sysusers",
            &[&layout.sysusers.display().to_string()],
        ));
    }
    // Handed to the service's user before anything is put in them: what
    // root creates inside would lock the server out.
    for path in [
        layout.config_dir.clone(),
        layout.config_dir.join("tls"),
        layout.data_dir.clone(),
        layout.data_dir.join("workspaces"),
    ] {
        actions.push(Action::MakeDir {
            path,
            mode: 0o700,
            owner: Owner::Service,
        });
    }
    if !found.config_exists {
        actions.push(Action::WriteFile {
            path: layout.config_file(),
            content: wanted.initial_config.clone(),
            mode: 0o600,
            owner: Owner::Service,
        });
    }
    let mut pair_there = found.cert_exists && found.key_exists;
    if let Some((cert_file, key_file)) = pair
        && !pair_there
        && !wanted.names.is_empty()
    {
        actions.push(Action::MakePair {
            cert_file: cert_file.clone(),
            key_file: key_file.clone(),
            names: wanted.names.clone(),
        });
        pair_there = true;
    }
    if found.unit != Existing::Same {
        actions.push(Action::WriteFile {
            path: layout.unit.clone(),
            content: render_unit(&layout.binary),
            mode: 0o644,
            owner: Owner::Root,
        });
        actions.push(run("systemctl", &["daemon-reload"]));
        changed = true;
    }
    actions.push(run("systemctl", &["enable", UNIT]));

    let not_started = match pair {
        Some((cert_file, key_file)) if !pair_there => Some(format!(
            "not started: listen.mode is \"tls\" and there is no pair at {} and {}. Put yours there, readable by {USER}, or make one:\n  sudo -u {USER} {} tls self-signed --host <name>\nthen:\n  sudo systemctl start {UNIT}",
            cert_file.display(),
            key_file.display(),
            layout.binary.display()
        )),
        _ => None,
    };
    if not_started.is_none() {
        // A new binary or unit reaches a running service only by a restart;
        // without either, a start leaves a running service alone.
        let verb = if changed { "restart" } else { "start" };
        actions.push(run("systemctl", &[verb, UNIT]));
    }
    Ok(Plan {
        actions,
        not_started,
    })
}

/// The unit goes, and nothing else does.
///
/// # Errors
///
/// When the unit file is someone else's.
pub fn plan_remove(layout: &Layout, unit: Existing) -> Result<Vec<Action>, String> {
    match unit {
        Existing::Absent => Ok(Vec::new()),
        Existing::Foreign => Err(foreign(&layout.unit)),
        Existing::Same | Existing::Ours => Ok(vec![
            run("systemctl", &["disable", "--now", UNIT]),
            Action::RemoveFile {
                path: layout.unit.clone(),
            },
            run("systemctl", &["daemon-reload"]),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_host() -> Found {
        Found {
            running: "/home/me/passalong-server/target/release/passalong-server".into(),
            binary_installed: false,
            unit: Existing::Absent,
            sysusers: Existing::Absent,
            config_exists: false,
            tls: Some((
                "/etc/passalong-server/tls/cert.pem".into(),
                "/etc/passalong-server/tls/key.pem".into(),
            )),
            cert_exists: false,
            key_exists: false,
        }
    }

    fn installed() -> Found {
        Found {
            running: Layout::default().binary,
            unit: Existing::Same,
            sysusers: Existing::Same,
            config_exists: true,
            cert_exists: true,
            key_exists: true,
            ..empty_host()
        }
    }

    fn wanted(names: &[&str]) -> Wanted {
        Wanted {
            names: names.iter().map(ToString::to_string).collect(),
            initial_config: "the initial configuration".to_owned(),
        }
    }

    /// The plan in a line per action, for comparing at a glance.
    fn brief(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .map(|action| match action {
                Action::InstallBinary { to, .. } => format!("binary {}", to.display()),
                Action::MakeDir { path, mode, owner } => {
                    format!("dir {} {mode:o} {owner:?}", path.display())
                }
                Action::WriteFile {
                    path, mode, owner, ..
                } => format!("write {} {mode:o} {owner:?}", path.display()),
                Action::MakePair { names, .. } => format!("pair {}", names.join(",")),
                Action::RemoveFile { path } => format!("remove {}", path.display()),
                Action::Run { program, args } => format!("{program} {}", args.join(" ")),
            })
            .collect()
    }

    const DIRS: [&str; 4] = [
        "dir /etc/passalong-server 700 Service",
        "dir /etc/passalong-server/tls 700 Service",
        "dir /var/lib/passalong-server 700 Service",
        "dir /var/lib/passalong-server/workspaces 700 Service",
    ];

    #[test]
    fn an_empty_host_gets_everything_in_an_order_that_works() {
        let plan =
            plan_install(&Layout::default(), &empty_host(), &wanted(&["nas.example"])).unwrap();
        let mut expected = vec![
            "binary /usr/local/bin/passalong-server",
            // The user before the directories that are given to it.
            "write /etc/sysusers.d/passalong-server.conf 644 Root",
            "systemd-sysusers /etc/sysusers.d/passalong-server.conf",
        ];
        expected.extend(DIRS);
        expected.extend([
            // Inside directories that are the service's already.
            "write /etc/passalong-server/config.toml 600 Service",
            "pair nas.example",
            "write /etc/systemd/system/passalong-server.service 644 Root",
            "systemctl daemon-reload",
            "systemctl enable passalong-server.service",
            "systemctl restart passalong-server.service",
        ]);
        assert_eq!(brief(&plan.actions), expected);
        assert_eq!(plan.not_started, None);
        // The database is never in the plan: it is `serve`'s to create.
        assert!(!format!("{:?}", plan.actions).contains("sqlite"));
    }

    #[test]
    fn without_a_pair_and_without_names_it_is_enabled_and_not_started() {
        let plan = plan_install(&Layout::default(), &empty_host(), &wanted(&[])).unwrap();
        let brief = brief(&plan.actions);
        assert!(!brief.iter().any(|line| line.starts_with("pair")));
        assert!(brief.contains(&"systemctl enable passalong-server.service".to_owned()));
        assert!(
            !brief.iter().any(|line| line.contains("start")),
            "{brief:?}"
        );
        let why = plan.not_started.unwrap();
        for part in [
            "cert.pem",
            "sudo -u passalong-server /usr/local/bin/passalong-server tls self-signed",
            "sudo systemctl start passalong-server.service",
        ] {
            assert!(why.contains(part), "{why}");
        }
        // Plain mode needs no pair.
        let plain = Found {
            tls: None,
            ..empty_host()
        };
        let plan = plan_install(&Layout::default(), &plain, &wanted(&["ignored.example"])).unwrap();
        assert_eq!(plan.not_started, None);
        assert!(
            !self::brief(&plan.actions)
                .iter()
                .any(|line| line.starts_with("pair"))
        );
    }

    #[test]
    fn a_second_install_changes_nothing() {
        let plan =
            plan_install(&Layout::default(), &installed(), &wanted(&["nas.example"])).unwrap();
        let mut expected: Vec<&str> = DIRS.to_vec();
        expected.extend([
            "systemctl enable passalong-server.service",
            // Leaves a running service alone.
            "systemctl start passalong-server.service",
        ]);
        assert_eq!(brief(&plan.actions), expected);
    }

    #[test]
    fn the_same_binary_is_not_installed_again() {
        // Run again from the build directory, with nothing rebuilt.
        let again = Found {
            running: empty_host().running,
            binary_installed: true,
            ..installed()
        };
        let brief = brief(
            &plan_install(&Layout::default(), &again, &wanted(&[]))
                .unwrap()
                .actions,
        );
        assert!(
            !brief.iter().any(|line| line.starts_with("binary")),
            "{brief:?}"
        );
        assert_eq!(
            brief.last().unwrap(),
            "systemctl start passalong-server.service"
        );
        // Rebuilt: installed, and the service restarted to run it.
        let rebuilt = Found {
            binary_installed: false,
            ..again
        };
        let brief = self::brief(
            &plan_install(&Layout::default(), &rebuilt, &wanted(&[]))
                .unwrap()
                .actions,
        );
        assert_eq!(brief[0], "binary /usr/local/bin/passalong-server");
        assert_eq!(
            brief.last().unwrap(),
            "systemctl restart passalong-server.service"
        );
    }

    #[test]
    fn what_is_there_is_kept() {
        // A configuration and a pair of the operator's own.
        let own = Found {
            config_exists: true,
            cert_exists: true,
            key_exists: true,
            ..empty_host()
        };
        let plan = plan_install(&Layout::default(), &own, &wanted(&["nas.example"])).unwrap();
        let brief = brief(&plan.actions);
        assert!(
            !brief.iter().any(|line| line.contains("config.toml")),
            "{brief:?}"
        );
        assert!(
            !brief.iter().any(|line| line.starts_with("pair")),
            "{brief:?}"
        );
        assert_eq!(plan.not_started, None);

        // An older unit of ours is replaced, and the service restarted.
        let older = Found {
            unit: Existing::Ours,
            ..installed()
        };
        let brief = self::brief(
            &plan_install(&Layout::default(), &older, &wanted(&[]))
                .unwrap()
                .actions,
        );
        assert!(
            brief.contains(
                &"write /etc/systemd/system/passalong-server.service 644 Root".to_owned()
            )
        );
        assert_eq!(
            brief.last().unwrap(),
            "systemctl restart passalong-server.service"
        );
    }

    #[test]
    fn what_is_in_the_way_stops_everything() {
        for found in [
            Found {
                unit: Existing::Foreign,
                ..empty_host()
            },
            Found {
                sysusers: Existing::Foreign,
                ..empty_host()
            },
        ] {
            let said = plan_install(&Layout::default(), &found, &wanted(&[])).unwrap_err();
            assert!(said.contains("was not written by"), "{said}");
        }
        for (cert_exists, key_exists) in [(true, false), (false, true)] {
            let half = Found {
                cert_exists,
                key_exists,
                ..empty_host()
            };
            let said =
                plan_install(&Layout::default(), &half, &wanted(&["nas.example"])).unwrap_err();
            assert!(said.contains("half a pair"), "{said}");
        }
    }

    #[test]
    fn a_file_is_ours_by_its_marker() {
        let unit = render_unit(&Layout::default().binary);
        assert_eq!(Existing::of(None, &unit), Existing::Absent);
        assert_eq!(Existing::of(Some(&unit), &unit), Existing::Same);
        let older = unit.replace("TimeoutStopSec=45", "TimeoutStopSec=90");
        assert_eq!(Existing::of(Some(&older), &unit), Existing::Ours);
        assert_eq!(
            Existing::of(Some("[Service]\nExecStart=/bin/true\n"), &unit),
            Existing::Foreign
        );
    }

    #[test]
    fn remove_takes_the_unit_and_nothing_else() {
        let layout = Layout::default();
        assert_eq!(
            brief(&plan_remove(&layout, Existing::Same).unwrap()),
            [
                "systemctl disable --now passalong-server.service",
                "remove /etc/systemd/system/passalong-server.service",
                "systemctl daemon-reload",
            ]
        );
        assert_eq!(
            plan_remove(&layout, Existing::Ours).unwrap(),
            plan_remove(&layout, Existing::Same).unwrap()
        );
        // Twice is not an error.
        assert!(plan_remove(&layout, Existing::Absent).unwrap().is_empty());
        assert!(plan_remove(&layout, Existing::Foreign).is_err());
    }
}
