//! The binary, driven as an operator drives it (PLAN-00003, REQ-09). Every
//! command line in `docs/usage.md` that belongs to this slice is run here.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;

use passalong_server_core::clock::SystemClock;
use passalong_server_core::control::Control;
use passalong_server_core::error::ApiError;
use passalong_server_core::random::OsRandom;
use passalong_server_core::workspace::Role;

const BIN: &str = env!("CARGO_BIN_EXE_passalong-server");

struct Host {
    dir: tempfile::TempDir,
    /// Everything any command printed, to search for secrets afterwards.
    transcript: std::cell::RefCell<String>,
}

impl Host {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            transcript: Default::default(),
        }
    }
    fn config(&self) -> PathBuf {
        self.dir.path().join("etc/config.toml")
    }
    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }
    fn raw(&self, args: &[&str]) -> Output {
        let output = Command::new(BIN)
            .args(args)
            .env_remove("PASSALONG_SERVER_CONFIG_FILE")
            .env("PASSALONG_SERVER_LOG_LEVEL", "debug")
            .env("HOME", self.dir.path())
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap();
        let mut transcript = self.transcript.borrow_mut();
        transcript.push_str(&format!("$ passalong-server {}\n", args.join(" ")));
        transcript.push_str(&String::from_utf8_lossy(&output.stdout));
        transcript.push_str(&String::from_utf8_lossy(&output.stderr));
        output
    }
    /// A command with `--config`, which must succeed; its standard output.
    fn run(&self, args: &[&str]) -> String {
        let config = self.config();
        let mut all = vec!["--config", config.to_str().unwrap()];
        all.extend(args);
        let output = self.raw(&all);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    /// A command that must fail with `code`; its standard error.
    fn fails(&self, args: &[&str], code: i32) -> String {
        let config = self.config();
        let mut all = vec!["--config", config.to_str().unwrap()];
        all.extend(args);
        let output = self.raw(&all);
        assert_eq!(
            output.status.code(),
            Some(code),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8(output.stderr).unwrap()
    }
    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["--json"];
        all.extend(args);
        serde_json::from_str(&self.run(&all)).unwrap()
    }
    fn control(&self) -> Control {
        Control::open(
            self.data().join("control.sqlite"),
            Duration::from_secs(5),
            Arc::new(SystemClock),
            Box::new(OsRandom),
        )
        .unwrap()
    }
}

fn init(host: &Host) {
    let output = host.raw(&[
        "init",
        "--data-dir",
        host.data().to_str().unwrap(),
        "--config-file",
        host.config().to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every file below `dir`, read raw.
fn every_file(dir: &Path, found: &mut Vec<(PathBuf, Vec<u8>)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            every_file(&path, found);
        } else {
            found.push((path.clone(), std::fs::read(&path).unwrap()));
        }
    }
}

#[test]
fn an_operators_session_from_an_empty_host() {
    let host = Host::new();
    init(&host);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(host.data()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    // A second `init` does not overwrite what is there.
    let again = host.raw(&[
        "init",
        "--data-dir",
        host.data().to_str().unwrap(),
        "--config-file",
        host.config().to_str().unwrap(),
    ]);
    assert_eq!(again.status.code(), Some(1));

    // Workspaces.
    let out = host.run(&["workspace", "create", "home", "--quota", "20GiB"]);
    assert!(out.contains("home"), "{out}");
    host.run(&["workspace", "create", "work"]);
    assert!(
        host.fails(&["workspace", "create", "home"], 1)
            .contains("already")
    );
    assert!(
        host.fails(&["workspace", "create", "Not A Name"], 1)
            .contains("lower-case")
    );
    let listed = host.json(&["workspace", "list"]);
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["home", "work"]);
    assert_eq!(listed[0]["quotaBytes"], "21474836480");
    assert!(
        listed[1]["quotaBytes"].is_null(),
        "the default quota is said as null"
    );

    // A key, shown once.
    let out = host.run(&[
        "key",
        "create",
        "--workspace",
        "home",
        "--label",
        "laptop",
        "--expires",
        "90d",
    ]);
    let token = out
        .split_whitespace()
        .find(|word| word.starts_with("pal_"))
        .expect("the key is printed")
        .to_owned();
    assert_eq!(token.len(), 81);
    assert_eq!(out.matches("pal_").count(), 1, "printed once:\n{out}");
    let key_id = token[4..16].to_owned();
    let who = host.control().authenticate(&token).unwrap();
    assert_eq!(who.caller.role, Role::ReadWrite);

    // Without `--expires` a key gets 90 days and says so; never is a choice.
    let out = host.run(&[
        "key",
        "create",
        "--workspace",
        "home",
        "--label",
        "phone",
        "--read-only",
    ]);
    assert!(out.contains("90 days"), "{out}");
    let forever = host.run(&[
        "key",
        "create",
        "--workspace",
        "work",
        "--label",
        "nas",
        "--never",
    ]);
    assert!(forever.contains("never"), "{forever}");
    assert!(
        !host
            .fails(
                &[
                    "key",
                    "create",
                    "--workspace",
                    "home",
                    "--label",
                    "x",
                    "--never",
                    "--expires",
                    "1d"
                ],
                2
            )
            .is_empty()
    );

    // Listings: a table for people, JSON for scripts; never a secret.
    let table = host.run(&["key", "list"]);
    assert!(
        table.contains(&key_id)
            && table.contains("laptop")
            && table.contains("read-only")
            && table.contains("active"),
        "{table}"
    );
    let keys = host.json(&["key", "list", "--workspace", "home"]);
    assert_eq!(keys.as_array().unwrap().len(), 2);
    assert_eq!(keys[0]["id"], key_id.as_str());
    assert_eq!(keys[0]["state"], "active");
    assert!(keys[0]["expiresAt"].as_str().unwrap().ends_with('Z'));
    assert!(keys[0].get("secret").is_none() && keys[0].get("hash").is_none());

    // The workspace, as the operator sees it.
    let shown = host.json(&["workspace", "show", "home"]);
    assert_eq!(shown["name"], "home");
    assert_eq!(shown["encryption"]["state"], "plaintext");
    assert_eq!(shown["items"], 0);
    assert_eq!(shown["keys"].as_array().unwrap().len(), 2);
    assert!(
        host.run(&["workspace", "show", "home"])
            .contains("plaintext")
    );

    // More time for the same secret; then no more at all.
    host.run(&["key", "extend", &key_id, "--expires", "30d"]);
    host.run(&["key", "extend", &key_id, "--never"]);
    assert!(host.control().authenticate(&token).is_ok());
    host.run(&["key", "revoke", &key_id]);
    assert_eq!(
        host.control().authenticate(&token).unwrap_err(),
        ApiError::KeyRevoked
    );
    assert!(host.run(&["key", "list"]).contains("revoked"));
    assert!(
        host.fails(&["key", "revoke", "000000000000"], 1)
            .contains("no key")
    );
    assert_eq!(
        host.json(&["key", "prune", "--older-than", "30d"])["pruned"],
        0
    );
    host.run(&["key", "delete", &key_id]);
    assert_eq!(
        host.json(&["key", "list", "--workspace", "home"])
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // No rewrite is open.
    assert!(
        host.fails(&["rewrite", "show", "home"], 1)
            .contains("no rewrite")
    );
    assert!(
        host.fails(&["rewrite", "abort", "home"], 1)
            .contains("no rewrite")
    );

    // Everything is in order, and says so.
    let checked = host.run(&["check"]);
    assert!(
        checked.contains("ok") && !checked.to_lowercase().contains("fail"),
        "{checked}"
    );

    // Deleting asks to be meant.
    assert!(
        host.fails(&["workspace", "delete", "work"], 1)
            .contains("--yes")
    );
    host.run(&["workspace", "delete", "work", "--yes"]);
    assert_eq!(
        host.json(&["workspace", "list"]).as_array().unwrap().len(),
        1
    );
    assert_eq!(
        std::fs::read_dir(host.data().join("workspaces"))
            .unwrap()
            .count(),
        1,
        "the deleted workspace's directory is gone"
    );

    // `service` changes the system: not as root, it refuses, and does
    // so without needing a configuration or the data directory.
    // Never as root: there the command would do what it says, to this machine.
    let root = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").unwrap().uid() == 0
    };
    for what in ["install", "remove"].into_iter().filter(|_| !root) {
        let output = host.raw(&["service", what]);
        assert_eq!(output.status.code(), Some(1));
        let said = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            said.contains("must run as root") && said.contains("sudo "),
            "{said}"
        );
    }
    // Usage errors are 2.
    host.fails(&["key", "create"], 2);
    host.fails(&["no-such-command"], 2);

    // The secret left the process once: in the output of that `key create`.
    let secret = &token[17..];
    let transcript = host.transcript.borrow();
    assert_eq!(
        transcript.matches(&secret[..16]).count(),
        1,
        "the secret appears once in everything that was printed or logged"
    );
    let mut files = Vec::new();
    every_file(host.dir.path(), &mut files);
    assert!(files.len() >= 2);
    for (path, bytes) in files {
        assert!(
            !bytes
                .windows(16)
                .any(|window| window == &secret.as_bytes()[..16]),
            "{} holds part of the secret",
            path.display()
        );
    }
}

#[test]
fn a_pair_is_made_once_and_its_pin_printed() {
    use std::os::unix::fs::PermissionsExt;
    let host = Host::new();
    init(&host);
    // `init` expects the pair beside the configuration file.
    let tls = host.dir.path().join("etc/tls");
    let (cert, key) = (tls.join("cert.pem"), tls.join("key.pem"));

    let nothing = host.fails(&["tls", "fingerprint"], 1);
    assert!(nothing.contains("cert.pem"), "{nothing}");
    assert!(nothing.contains("tls self-signed"), "{nothing}");
    host.fails(&["tls", "self-signed"], 2);
    let odd = host.fails(&["tls", "self-signed", "--host", "https://nas"], 1);
    assert!(
        odd.contains("neither a host name nor an IP address"),
        "{odd}"
    );
    assert!(!cert.exists() && !key.exists());

    let made = host.run(&[
        "tls",
        "self-signed",
        "--host",
        "nas.example",
        "--ip",
        "192.0.2.4",
        "--ip",
        "2001:db8::4",
    ]);
    let pin = made
        .lines()
        .find_map(|line| line.trim().strip_prefix("tls_pin = "))
        .unwrap_or_else(|| panic!("no pin in: {made}"))
        .trim_matches('"')
        .to_owned();
    assert!(pin.starts_with("sha256/") && pin.len() == 51, "{pin}");
    for file in [&cert, &key] {
        assert!(made.contains(file.to_str().unwrap()), "{made}");
    }
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&key), 0o600);
    assert_eq!(mode(&tls), 0o700);
    assert!(
        std::fs::read_to_string(&cert)
            .unwrap()
            .contains("BEGIN CERTIFICATE")
    );

    assert_eq!(host.run(&["tls", "fingerprint"]).trim(), pin);
    assert_eq!(host.json(&["tls", "fingerprint"])["pin"], pin.as_str());

    // Never over a pair that is there: a client may have pinned it.
    let before = std::fs::read(&key).unwrap();
    let again = host.fails(&["tls", "self-signed", "--host", "nas.example"], 1);
    assert!(again.contains("exists already"), "{again}");
    assert_eq!(std::fs::read(&key).unwrap(), before);
    // Half a pair is not completed either: the key would not be its key.
    std::fs::remove_file(&cert).unwrap();
    host.fails(&["tls", "self-signed", "--host", "nas.example"], 1);
    assert!(!cert.exists());

    assert!(!host.transcript.borrow().contains("PRIVATE KEY"));
}

#[test]
fn without_a_configuration_a_command_says_where_it_looked() {
    let host = Host::new();
    let output = host.raw(&["workspace", "list"]);
    assert_eq!(output.status.code(), Some(1));
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("init") && said.contains("config.toml"),
        "{said}"
    );
    // A file with a key nobody knows is refused by that key's name.
    init(&host);
    let text = std::fs::read_to_string(host.config()).unwrap();
    std::fs::write(host.config(), format!("{text}\n[server2]\nx = 1\n")).unwrap();
    assert!(host.fails(&["workspace", "list"], 1).contains("server2"));
}

#[test]
fn a_dead_rewrite_session_is_shown_and_aborted_from_the_host() {
    use passalong_server_core::ids::KeyId;
    use passalong_server_core::ledger::SqliteLedger;
    use passalong_server_core::rewrite::{RewriteKind, RewriteRequest};
    use passalong_server_core::shelf::FsShelf;
    use passalong_server_core::workspace::{Engine, Limits};

    let host = Host::new();
    init(&host);
    host.run(&["workspace", "create", "home"]);
    let out = host.run(&[
        "key",
        "create",
        "--workspace",
        "home",
        "--label",
        "laptop",
        "--never",
    ]);
    let token = out
        .split_whitespace()
        .find(|word| word.starts_with("pal_"))
        .unwrap()
        .to_owned();
    let who = host.control().authenticate(&token).unwrap();

    // A device begins a migration and is never heard of again.
    {
        let engine = Engine::open(
            FsShelf::open(host.data().join("workspaces").join(who.workspace.as_str())).unwrap(),
            SqliteLedger::open(host.data().join("control.sqlite"), Duration::from_secs(5)).unwrap(),
            who.workspace.clone(),
            Arc::new(SystemClock),
            Box::new(OsRandom),
            Limits {
                // Made-up ids: the content check has tests of its own.
                check_plaintext_content: false,
                ..Limits::default()
            },
        )
        .unwrap();
        let request = RewriteRequest {
            kind: RewriteKind::Migrate,
            expected_key_id: None,
            new_key_id: KeyId::parse("bb").unwrap(),
            new_header: b"{}".to_vec(),
        };
        engine.begin_rewrite(&who.caller, request).unwrap();
    }

    let shown = host.run(&["rewrite", "show", "home"]);
    assert!(
        shown.contains("migrate") && shown.contains(&token[4..16]),
        "{shown}"
    );
    assert_eq!(
        host.json(&["workspace", "show", "home"])["encryption"]["state"],
        "rewriting"
    );
    // Its lease still runs: not without being told to.
    assert!(
        host.fails(&["rewrite", "abort", "home"], 1)
            .contains("--force")
    );
    assert!(
        host.fails(&["workspace", "delete", "home", "--yes"], 1)
            .contains("rewrite")
    );
    host.run(&["rewrite", "abort", "home", "--force"]);
    assert_eq!(
        host.json(&["workspace", "show", "home"])["encryption"]["state"],
        "plaintext"
    );
}
