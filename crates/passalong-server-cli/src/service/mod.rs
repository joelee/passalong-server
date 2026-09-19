//! `service install` and `service remove`: the server as a systemd system
//! service. They run as root, and so, unlike every other command, neither
//! under the owner rule nor with a configuration loaded: there may be no
//! data directory yet, and no file. What they decide is [`plan`]'s; what
//! they touch goes through [`system::System`].

pub mod plan;
pub mod system;
pub mod unit;

use std::fmt::Write as _;
use std::path::Path;

use passalong_server_core::config::{self, ListenMode};

use crate::cli::ServiceCommand;
use crate::commands::Done;
use plan::{Action, Existing, Found, Layout, Owner, Wanted};
use system::System;
use unit::{UNIT, USER};

fn must_be_root(system: &dyn System, what: &str) -> Result<(), String> {
    if system.is_root() {
        return Ok(());
    }
    let binary = system.running_binary().map_or_else(
        |_| "passalong-server".to_owned(),
        |path| path.display().to_string(),
    );
    Err(format!(
        "`service {what}` changes the system and must run as root. Nothing was done. Run:\n  sudo {binary} service {what}"
    ))
}

/// Where the pair is expected, if the configuration serves TLS itself.
fn pair_of(
    text: &str,
    file: &Path,
) -> Result<Option<(std::path::PathBuf, std::path::PathBuf)>, String> {
    let config = config::parse(text, file, &config::Process).map_err(|err| err.to_string())?;
    Ok(match (config.listen.mode, config.tls) {
        (ListenMode::Tls, Some(tls)) => Some((tls.cert_file, tls.key_file)),
        (ListenMode::Tls, None) => {
            return Err(format!(
                "{}: listen.mode is \"tls\" and there is no [tls] section",
                file.display()
            ));
        }
        (ListenMode::Plain, _) => None,
    })
}

fn look(system: &dyn System, layout: &Layout, wanted: &Wanted) -> Result<Found, String> {
    let file = layout.config_file();
    let there = system.read(&file);
    let tls = pair_of(there.as_deref().unwrap_or(&wanted.initial_config), &file)?;
    Ok(Found {
        binary_installed: system.same_content(&system.running_binary()?, &layout.binary),
        running: system.running_binary()?,
        unit: Existing::of(
            system.read(&layout.unit).as_deref(),
            &unit::render_unit(&layout.binary),
        ),
        sysusers: Existing::of(
            system.read(&layout.sysusers).as_deref(),
            &unit::render_sysusers(&layout.data_dir),
        ),
        config_exists: there.is_some() || system.exists(&file),
        cert_exists: tls.as_ref().is_some_and(|(cert, _)| system.exists(cert)),
        key_exists: tls.as_ref().is_some_and(|(_, key)| system.exists(key)),
        tls,
    })
}

/// Does `action`, and says so in `done`.
fn perform(system: &dyn System, action: &Action, done: &mut String) -> Result<(), String> {
    let ids = |owner: Owner| match owner {
        Owner::Root => Ok(Some((0, 0))),
        Owner::Service => system.user_ids(USER).map(Some).ok_or_else(|| {
            format!("there is no user `{USER}`, though systemd-sysusers was asked for one")
        }),
    };
    match action {
        Action::InstallBinary { from, to } => {
            system.install_binary(from, to)?;
            let _ = writeln!(done, "installed {}", to.display());
        }
        Action::MakeDir { path, mode, owner } => {
            system.make_dir(path, *mode, ids(*owner)?)?;
            let _ = writeln!(done, "directory {} is {USER}'s alone", path.display());
        }
        Action::WriteFile {
            path,
            content,
            mode,
            owner,
        } => {
            system.write_file(path, content.as_bytes(), *mode, ids(*owner)?)?;
            let _ = writeln!(done, "wrote {}", path.display());
        }
        Action::MakePair {
            cert_file,
            key_file,
            names,
        } => {
            let pair = passalong_server_api::tls::self_signed(names, system.now())
                .map_err(|err| err.to_string())?;
            let pin = passalong_server_api::tls::pin_of_pem(pair.cert_pem.as_bytes())
                .map_err(|err| err.to_string())?;
            let owner = ids(Owner::Service)?;
            system.write_file(key_file, pair.key_pem.as_bytes(), 0o600, owner)?;
            system.write_file(cert_file, pair.cert_pem.as_bytes(), 0o644, owner)?;
            let _ = writeln!(
                done,
                "made a self-signed pair for {}; clients connect by its pin:\n  tls_pin = \"{pin}\"",
                names.join(", ")
            );
        }
        Action::RemoveFile { path } => {
            system.remove_file(path)?;
            let _ = writeln!(done, "removed {}", path.display());
        }
        Action::Run { program, args } => {
            system.run(program, args)?;
            let _ = writeln!(done, "ran {program} {}", args.join(" "));
        }
    }
    Ok(())
}

fn perform_all(system: &dyn System, actions: &[Action]) -> Result<String, String> {
    let mut done = String::new();
    for action in actions {
        if let Err(err) = perform(system, action, &mut done) {
            let before = if done.is_empty() {
                "Nothing was done before that.".to_owned()
            } else {
                format!("Done before that, and left as it is:\n{done}")
            };
            return Err(format!(
                "{err}\n{before}Running the command again takes up where this stopped."
            ));
        }
    }
    Ok(done)
}

pub fn service(system: &dyn System, layout: &Layout, command: &ServiceCommand) -> Done {
    match command {
        ServiceCommand::Install { host, ip } => {
            must_be_root(system, "install")?;
            let mut names = host.clone();
            names.extend(ip.iter().map(ToString::to_string));
            let wanted = Wanted {
                names,
                initial_config: config::initial_file(&layout.data_dir, &layout.config_dir),
            };
            let found = look(system, layout, &wanted)?;
            let plan = plan::plan_install(layout, &found, &wanted)?;
            let mut said = perform_all(system, &plan.actions)?;
            let binary = layout.binary.display();
            match &plan.not_started {
                None => {
                    let _ = writeln!(said, "\n{UNIT} is enabled and running.");
                }
                Some(why) => {
                    let _ = writeln!(said, "\n{UNIT} is enabled, and {why}");
                }
            }
            let _ = write!(
                said,
                "\nCommands run as the service's user, never as root:\n  sudo -u {USER} {binary} workspace create home\n  sudo -u {USER} {binary} key create --workspace home --label laptop\nLogs:\n  journalctl -u {UNIT} -f\n"
            );
            Ok(said)
        }
        ServiceCommand::Remove => {
            must_be_root(system, "remove")?;
            let unit = Existing::of(
                system.read(&layout.unit).as_deref(),
                &unit::render_unit(&layout.binary),
            );
            let actions = plan::plan_remove(layout, unit)?;
            let mut said = perform_all(system, &actions)?;
            if actions.is_empty() {
                let _ = writeln!(
                    said,
                    "{} is not there; nothing to remove.",
                    layout.unit.display()
                );
            }
            let _ = write!(
                said,
                "\nLeft as they are, for you to keep or delete:\n  {}  every workspace's items, and the API keys\n  {}  the configuration and the TLS pair\n  {}\n  the user {USER} ({})\n",
                layout.data_dir.display(),
                layout.config_dir.display(),
                layout.binary.display(),
                layout.sysusers.display(),
            );
            Ok(said)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// A host that remembers what it was asked and holds files in a map.
    #[derive(Default)]
    struct Fake {
        root: bool,
        files: RefCell<BTreeMap<PathBuf, String>>,
        asked: RefCell<Vec<String>>,
        /// A program that fails when run.
        broken: Option<&'static str>,
        /// Whether `systemd-sysusers` makes the user.
        no_user: bool,
        users: RefCell<Vec<String>>,
        /// Whether the binary in its place is this one.
        installed: std::cell::Cell<bool>,
    }

    impl Fake {
        fn root() -> Self {
            Self {
                root: true,
                ..Self::default()
            }
        }
        fn asked(&self) -> Vec<String> {
            self.asked.borrow().clone()
        }
        fn note(&self, what: String) {
            self.asked.borrow_mut().push(what);
        }
        fn file(&self, path: &str) -> Option<String> {
            self.files.borrow().get(Path::new(path)).cloned()
        }
    }

    impl System for Fake {
        fn is_root(&self) -> bool {
            self.root
        }
        fn running_binary(&self) -> Result<PathBuf, String> {
            Ok("/home/me/target/release/passalong-server".into())
        }
        fn read(&self, path: &Path) -> Option<String> {
            self.files.borrow().get(path).cloned()
        }
        fn exists(&self, path: &Path) -> bool {
            self.files.borrow().contains_key(path)
        }
        fn same_content(&self, _: &Path, other: &Path) -> bool {
            let _ = other;
            self.installed.get()
        }
        fn user_ids(&self, name: &str) -> Option<system::Ids> {
            self.users
                .borrow()
                .iter()
                .any(|user| user == name)
                .then_some((987, 985))
        }
        fn make_dir(
            &self,
            path: &Path,
            mode: u32,
            owner: Option<system::Ids>,
        ) -> Result<(), String> {
            self.note(format!("dir {} {mode:o} {owner:?}", path.display()));
            Ok(())
        }
        fn write_file(
            &self,
            path: &Path,
            content: &[u8],
            mode: u32,
            owner: Option<system::Ids>,
        ) -> Result<(), String> {
            self.note(format!("write {} {mode:o} {owner:?}", path.display()));
            self.files.borrow_mut().insert(
                path.to_owned(),
                String::from_utf8_lossy(content).into_owned(),
            );
            Ok(())
        }
        fn install_binary(&self, from: &Path, to: &Path) -> Result<(), String> {
            self.note(format!("binary {} -> {}", from.display(), to.display()));
            self.installed.set(true);
            Ok(())
        }
        fn remove_file(&self, path: &Path) -> Result<(), String> {
            self.note(format!("remove {}", path.display()));
            self.files.borrow_mut().remove(path);
            Ok(())
        }
        fn run(&self, program: &str, args: &[String]) -> Result<(), String> {
            let shown = format!("{program} {}", args.join(" "));
            if self.broken.is_some_and(|broken| shown.contains(broken)) {
                return Err(format!("`{shown}` failed: no"));
            }
            if program == "systemd-sysusers" && !self.no_user {
                self.users.borrow_mut().push(USER.to_owned());
            }
            self.note(shown);
            Ok(())
        }
        fn now(&self) -> u64 {
            1_800_000_000
        }
    }

    fn install(names: &[&str]) -> ServiceCommand {
        ServiceCommand::Install {
            host: names.iter().map(ToString::to_string).collect(),
            ip: vec!["192.0.2.4".parse().unwrap()],
        }
    }

    #[test]
    fn whoever_is_not_root_is_refused_and_nothing_is_done() {
        let fake = Fake::default();
        for (command, what) in [
            (install(&["nas.example"]), "install"),
            (ServiceCommand::Remove, "remove"),
        ] {
            let said = service(&fake, &Layout::default(), &command).unwrap_err();
            assert!(
                said.contains(&format!(
                    "sudo /home/me/target/release/passalong-server service {what}"
                )),
                "{said}"
            );
        }
        assert!(fake.asked().is_empty());
    }

    #[test]
    fn an_empty_host_is_set_up_and_a_second_run_changes_nothing() {
        let fake = Fake::root();
        let said = service(&fake, &Layout::default(), &install(&["nas.example"])).unwrap();
        let asked = fake.asked();
        assert_eq!(
            asked[0],
            "binary /home/me/target/release/passalong-server -> /usr/local/bin/passalong-server"
        );
        assert_eq!(
            asked[1],
            "write /etc/sysusers.d/passalong-server.conf 644 Some((0, 0))"
        );
        assert_eq!(
            asked[2],
            "systemd-sysusers /etc/sysusers.d/passalong-server.conf"
        );
        // To the user that the step before made.
        assert_eq!(asked[3], "dir /etc/passalong-server 700 Some((987, 985))");
        assert!(
            asked.contains(
                &"write /etc/passalong-server/config.toml 600 Some((987, 985))".to_owned()
            )
        );
        // The key before the certificate, and for its owner's eyes only.
        let key = asked
            .iter()
            .position(|a| a == "write /etc/passalong-server/tls/key.pem 600 Some((987, 985))")
            .unwrap();
        let cert = asked
            .iter()
            .position(|a| a == "write /etc/passalong-server/tls/cert.pem 644 Some((987, 985))")
            .unwrap();
        assert!(key < cert);
        assert_eq!(
            asked.last().unwrap(),
            "systemctl restart passalong-server.service"
        );
        assert!(!asked.iter().any(|a| a.contains("sqlite")));

        // The files are what the repository shows.
        assert_eq!(
            fake.file("/etc/systemd/system/passalong-server.service")
                .unwrap(),
            unit::render_unit(Path::new("/usr/local/bin/passalong-server"))
        );
        let config = fake.file("/etc/passalong-server/config.toml").unwrap();
        assert!(config.contains("data_dir = \"/var/lib/passalong-server\""));
        assert!(config.contains("cert_file = \"/etc/passalong-server/tls/cert.pem\""));
        let pin = passalong_server_api::tls::pin_of_pem(
            fake.file("/etc/passalong-server/tls/cert.pem")
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        assert!(said.contains(&format!("tls_pin = \"{pin}\"")), "{said}");
        assert!(said.contains("enabled and running"), "{said}");
        assert!(
            said.contains("sudo -u passalong-server /usr/local/bin/passalong-server key create")
        );
        assert!(!said.contains("PRIVATE KEY"));

        // Again, now from the installed binary's point of view or not: the
        // files are the same, so nothing is written but directories' modes.
        let before = fake.files.borrow().clone();
        fake.asked.borrow_mut().clear();
        service(&fake, &Layout::default(), &install(&["other.example"])).unwrap();
        assert_eq!(*fake.files.borrow(), before, "a file changed");
        let again = fake.asked();
        assert!(
            !again
                .iter()
                .any(|a| a.starts_with("write") || a.starts_with("binary")),
            "{again:?}"
        );
        assert_eq!(
            again.last().unwrap(),
            "systemctl start passalong-server.service"
        );
        assert!(
            !again
                .iter()
                .any(|a| a.contains("daemon-reload") || a.contains("sysusers")),
            "{again:?}"
        );
    }

    #[test]
    fn without_names_it_is_enabled_and_says_what_is_missing() {
        let fake = Fake::root();
        let said = service(
            &fake,
            &Layout::default(),
            &ServiceCommand::Install {
                host: vec![],
                ip: vec![],
            },
        )
        .unwrap();
        assert!(said.contains("is enabled, and not started"), "{said}");
        assert!(said.contains("tls self-signed --host <name>"), "{said}");
        assert!(!fake.asked().iter().any(|a| a.contains("start")));
        assert_eq!(fake.file("/etc/passalong-server/tls/key.pem"), None);
    }

    #[test]
    fn the_operators_own_configuration_is_read_and_kept() {
        let fake = Fake::root();
        let plain = "[server]\ndata_dir = \"/var/lib/passalong-server\"\n[listen]\naddress = \"127.0.0.1:8080\"\nmode = \"plain\"\n";
        fake.files
            .borrow_mut()
            .insert("/etc/passalong-server/config.toml".into(), plain.to_owned());
        let said = service(&fake, &Layout::default(), &install(&["nas.example"])).unwrap();
        assert_eq!(
            fake.file("/etc/passalong-server/config.toml").unwrap(),
            plain
        );
        // Plain mode: no pair is needed, none is made, and it starts.
        assert_eq!(fake.file("/etc/passalong-server/tls/key.pem"), None);
        assert!(said.contains("enabled and running"), "{said}");

        // One that does not parse stops the command before it does anything.
        let broken = Fake::root();
        broken.files.borrow_mut().insert(
            "/etc/passalong-server/config.toml".into(),
            "listen = 7".to_owned(),
        );
        assert!(service(&broken, &Layout::default(), &install(&[])).is_err());
        assert!(broken.asked().is_empty());
    }

    #[test]
    fn a_failure_stops_the_rest_and_says_what_was_done() {
        let fake = Fake {
            broken: Some("daemon-reload"),
            ..Fake::root()
        };
        let said = service(&fake, &Layout::default(), &install(&["nas.example"])).unwrap_err();
        assert!(said.contains("`systemctl daemon-reload` failed"), "{said}");
        assert!(
            said.contains("Done before that")
                && said.contains("wrote /etc/passalong-server/config.toml"),
            "{said}"
        );
        assert!(!fake.asked().iter().any(|a| a.contains("enable")));

        // `systemd-sysusers` ran and made no user: nothing is given to nobody.
        let fake = Fake {
            no_user: true,
            ..Fake::root()
        };
        let said = service(&fake, &Layout::default(), &install(&[])).unwrap_err();
        assert!(
            said.contains("there is no user `passalong-server`"),
            "{said}"
        );
        assert!(!fake.asked().iter().any(|a| a.starts_with("dir")));
    }

    #[test]
    fn remove_takes_the_unit_away_and_says_what_stays() {
        let fake = Fake::root();
        service(&fake, &Layout::default(), &install(&["nas.example"])).unwrap();
        fake.asked.borrow_mut().clear();
        let said = service(&fake, &Layout::default(), &ServiceCommand::Remove).unwrap();
        assert_eq!(
            fake.asked(),
            [
                "systemctl disable --now passalong-server.service",
                "remove /etc/systemd/system/passalong-server.service",
                "systemctl daemon-reload",
            ]
        );
        for kept in [
            "/var/lib/passalong-server",
            "/etc/passalong-server",
            "/usr/local/bin/passalong-server",
        ] {
            assert!(said.contains(kept), "{said}");
        }
        assert!(fake.file("/etc/passalong-server/config.toml").is_some());
        assert!(fake.file("/etc/passalong-server/tls/key.pem").is_some());

        // Twice is not an error; someone else's unit is not touched.
        fake.asked.borrow_mut().clear();
        let said = service(&fake, &Layout::default(), &ServiceCommand::Remove).unwrap();
        assert!(said.contains("nothing to remove"), "{said}");
        assert!(fake.asked().is_empty());
        fake.files.borrow_mut().insert(
            "/etc/systemd/system/passalong-server.service".into(),
            "[Service]\nExecStart=/bin/true\n".to_owned(),
        );
        assert!(service(&fake, &Layout::default(), &ServiceCommand::Remove).is_err());
        assert!(service(&fake, &Layout::default(), &install(&[])).is_err());
        assert!(fake.asked().is_empty());
    }
}
