//! What each command does. Every one returns what to print, or why not.

use std::io::BufRead;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use passalong_server_core::clock::{Clock, SystemClock};
use passalong_server_core::config::{self, Config, ListenMode, parse_duration, parse_size};
use passalong_server_core::control::{Control, KeyInfo, KeyState, WorkspaceInfo};
use passalong_server_core::error::ApiError;
use passalong_server_core::ledger::{SCHEMA_VERSION, SqliteLedger};
use passalong_server_core::random::OsRandom;
use passalong_server_core::rewrite::RewriteKind;
use passalong_server_core::shelf::FsShelf;
use passalong_server_core::workspace::{EncryptionState, Engine, Partition, Role};
use serde_json::{Value, json};

use crate::cli::{InitArgs, KeyCommand, RewriteCommand, TlsCommand, WorkspaceCommand};
use crate::output::{size, table, timestamp, when};

/// How long a command waits for the server, or another command, to finish a
/// transaction.
const BUSY: Duration = Duration::from_secs(10);

/// The default life of a key (PLAN-00003 D-02).
const DEFAULT_EXPIRY: &str = "90d";

pub type Done = Result<String, String>;

/// What a command needs once the configuration is read.
pub struct Host {
    pub config: Config,
    pub json: bool,
}

fn said<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default() + "\n"
}

impl Host {
    fn control(&self) -> Result<Control, String> {
        Control::open(
            self.config.control_database(),
            BUSY,
            Arc::new(SystemClock),
            Box::new(OsRandom),
        )
        .map_err(said)
    }

    /// Opens a workspace's engine, which also repairs what an unclean stop
    /// left.
    fn engine(&self, workspace: &WorkspaceInfo) -> Result<Engine<FsShelf, SqliteLedger>, String> {
        let open = || -> Result<_, ApiError> {
            Engine::open(
                FsShelf::open(self.config.workspace_dir(&workspace.id))?,
                SqliteLedger::open(self.config.control_database(), BUSY)?,
                workspace.id.clone(),
                Arc::new(SystemClock),
                Box::new(OsRandom),
                self.config.limits_for(workspace.quota_bytes),
            )
        };
        open().map_err(|err| format!("workspace `{}`: {err}; see the log", workspace.name))
    }

    fn print(&self, human: String, machine: Value) -> Done {
        Ok(if self.json { pretty(&machine) } else { human })
    }
}

// ---------- init ----------

/// Where `init` writes when it is not told: the configuration file and the
/// data directory. The file is where `PASSALONG_SERVER_CONFIG_FILE` says,
/// if it says: that is where every later command will look, and it is how
/// the container image points `init` at its configuration volume.
fn default_paths(env: &dyn config::Environment, root: bool) -> (PathBuf, PathBuf) {
    let set = |name: &str| env.var(name).filter(|value| !value.is_empty());
    let (file, data) = match (root, set("HOME").map(PathBuf::from)) {
        (false, Some(home)) => {
            let config = set("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
            (
                config.join("passalong-server/config.toml"),
                home.join(".local/share/passalong-server"),
            )
        }
        _ => (
            "/etc/passalong-server/config.toml".into(),
            "/var/lib/passalong-server".into(),
        ),
    };
    (set(config::FILE_VARIABLE).map_or(file, PathBuf::from), data)
}

fn private_dir(path: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .and_then(|()| std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)))
        .map_err(|err| format!("{}: {err}", path.display()))
}

pub fn init(args: &InitArgs) -> Done {
    let root = crate::owner::current_uid().is_ok_and(|uid| uid == 0);
    let (default_config, default_data) = default_paths(&config::Process, root);
    let file = args.config_file.clone().unwrap_or(default_config);
    let data_dir = args.data_dir.clone().unwrap_or(default_data);
    if file.exists() {
        return Err(format!(
            "{} exists already, and is left as it is",
            file.display()
        ));
    }
    let data_dir =
        std::path::absolute(&data_dir).map_err(|err| format!("{}: {err}", data_dir.display()))?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
    }
    private_dir(&data_dir)?;
    private_dir(&data_dir.join("workspaces"))?;
    let file = std::path::absolute(&file).map_err(|err| format!("{}: {err}", file.display()))?;
    let beside = file.parent().unwrap_or(Path::new("/"));
    let text = config::initial_file(&data_dir, beside);
    let config = config::parse(&text, &file, &config::Process).map_err(said)?;
    std::fs::write(&file, text).map_err(|err| format!("{}: {err}", file.display()))?;
    Control::open(
        config.control_database(),
        BUSY,
        Arc::new(SystemClock),
        Box::new(OsRandom),
    )
    .map_err(said)?;
    Ok(format!(
        "wrote {}\ncreated {}\nnext: passalong-server workspace create <name>\n",
        file.display(),
        data_dir.display()
    ))
}

// ---------- the audit trail ----------

/// `audit`: what the commands did to workspaces and keys. The trail holds
/// ids, labels, and names; a secret was never written to it.
pub fn audit(host: &Host, limit: u32) -> Done {
    let entries = host
        .control()?
        .audit(usize::try_from(limit).unwrap_or(usize::MAX))
        .map_err(said)?;
    let dash = |text: &Option<String>| text.clone().unwrap_or_else(|| "-".to_owned());
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|entry| {
            vec![
                timestamp(entry.at),
                entry.action.clone(),
                dash(&entry.workspace),
                dash(&entry.key_id),
                dash(&entry.detail),
            ]
        })
        .collect();
    let machine = entries
        .iter()
        .map(|entry| {
            json!({
                "at": passalong_server_core::clock::rfc3339(entry.at),
                "action": entry.action,
                "workspace": entry.workspace,
                "keyId": entry.key_id,
                "detail": entry.detail,
            })
        })
        .collect();
    host.print(
        table(&["WHEN", "ACTION", "WORKSPACE", "KEY", "DETAIL"], &rows),
        Value::Array(machine),
    )
}

// ---------- TLS ----------

/// Writes `text` to a file that must not exist yet, readable by its owner
/// alone from its first byte on.
fn write_new(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("{}: {err}", path.display()))
}

pub fn tls(host: &Host, command: &TlsCommand) -> Done {
    let files = host.config.tls.as_ref().ok_or(
        "the configuration has no [tls] section: set tls.cert_file and tls.key_file first",
    )?;
    match command {
        TlsCommand::Fingerprint => {
            let pem = std::fs::read(&files.cert_file).map_err(|err| {
                format!(
                    "{}: {err}. Make a pair with `passalong-server tls self-signed --host <name>`",
                    files.cert_file.display()
                )
            })?;
            let pin = passalong_server_api::tls::pin_of_pem(&pem)
                .map_err(|err| format!("{}: {err}", files.cert_file.display()))?;
            if host.json {
                Ok(pretty(&json!({ "pin": pin })))
            } else {
                Ok(format!("{pin}\n"))
            }
        }
        TlsCommand::Letsencrypt {
            host: names,
            email,
            docker,
        } => {
            if host.config.listen.mode == ListenMode::Plain {
                return Err("listen.mode is \"plain\": TLS ends at your reverse proxy, and the certificate is the proxy's to get and renew, not this server's".to_owned());
            }
            use std::os::unix::fs::MetadataExt;
            let data = &host.config.server.data_dir;
            let owner = std::fs::metadata(data)
                .map(|meta| format!("{}:{}", meta.uid(), meta.gid()))
                .map_err(|err| format!("{}: {err}", data.display()))?;
            let made = crate::letsencrypt::walkthrough(&crate::letsencrypt::Inputs {
                names: names.clone(),
                email: email.clone(),
                cert_file: files.cert_file.clone(),
                key_file: files.key_file.clone(),
                owner,
                docker: *docker,
            })?;
            if host.json {
                Ok(pretty(&json!({
                    "certbot": made.certbot,
                    "hookPath": crate::letsencrypt::HOOK_PATH,
                    "hook": made.hook,
                    "firstRun": made.first_run,
                    "certFile": files.cert_file.display().to_string(),
                    "keyFile": files.key_file.display().to_string(),
                })))
            } else {
                Ok(made.text)
            }
        }
        TlsCommand::SelfSigned { host: names, ip } => {
            // Never over a pair, or half of one: a client may have pinned it.
            for file in [&files.cert_file, &files.key_file] {
                if file.exists() {
                    return Err(format!(
                        "{} exists already, and is left as it is. Clients may have pinned it; remove both files yourself if you mean to replace them",
                        file.display()
                    ));
                }
            }
            let mut all = names.clone();
            all.extend(ip.iter().map(ToString::to_string));
            let pair =
                passalong_server_api::tls::self_signed(&all, SystemClock.now()).map_err(said)?;
            for file in [&files.cert_file, &files.key_file] {
                if let Some(parent) = file.parent().filter(|parent| !parent.exists()) {
                    private_dir(parent)?;
                }
            }
            write_new(&files.key_file, &pair.key_pem)?;
            write_new(&files.cert_file, &pair.cert_pem)?;
            let pin =
                passalong_server_api::tls::pin_of_pem(pair.cert_pem.as_bytes()).map_err(said)?;
            if host.json {
                return Ok(pretty(&json!({
                    "certFile": files.cert_file.display().to_string(),
                    "keyFile": files.key_file.display().to_string(),
                    "names": all,
                    "pin": pin,
                })));
            }
            Ok(format!(
                "wrote {}\nwrote {} (yours alone to read)\nfor {}\n\nNobody signed this certificate, so clients connect by its pin. In the client's configuration:\n\n  tls_pin = \"{pin}\"\n\n`passalong-server tls fingerprint` prints it again.\n",
                files.cert_file.display(),
                files.key_file.display(),
                all.join(", "),
            ))
        }
    }
}

// ---------- workspaces ----------

fn workspace_json(workspace: &WorkspaceInfo) -> Value {
    json!({
        "id": workspace.id.as_str(),
        "name": workspace.name,
        "quotaBytes": workspace.quota_bytes.map(|bytes| bytes.to_string()),
        "createdAt": timestamp(workspace.created_at),
    })
}

fn state_text(state: EncryptionState) -> &'static str {
    match state {
        EncryptionState::Plaintext => "plaintext",
        EncryptionState::Sealed => "sealed",
        EncryptionState::Rewriting => "rewriting",
    }
}

pub fn workspace(host: &Host, command: &WorkspaceCommand) -> Done {
    let control = host.control()?;
    match command {
        WorkspaceCommand::Create { name, quota } => {
            let quota = match quota.as_deref().map(parse_size).transpose()? {
                Some(None) => return Err("a quota cannot be unlimited: the disk is not".to_owned()),
                Some(Some(bytes)) => Some(bytes),
                None => None,
            };
            let made = control.create_workspace(name, quota).map_err(said)?;
            private_dir(&host.config.workspace_dir(&made.id))?;
            let quota_said = made.quota_bytes.map_or_else(
                || {
                    format!(
                        "{} (the default)",
                        size(host.config.limits.workspace_quota_bytes)
                    )
                },
                size,
            );
            host.print(
                format!("created workspace `{}`, quota {quota_said}\nnext: passalong-server key create --workspace {} --label <device>\n", made.name, made.name),
                workspace_json(&made),
            )
        }
        WorkspaceCommand::List => {
            let all = control.workspaces().map_err(said)?;
            let rows: Vec<Vec<String>> = all
                .iter()
                .map(|w| {
                    vec![
                        w.name.clone(),
                        w.quota_bytes.map_or_else(|| "default".to_owned(), size),
                        timestamp(w.created_at),
                        w.id.as_str().to_owned(),
                    ]
                })
                .collect();
            host.print(
                table(&["NAME", "QUOTA", "CREATED", "ID"], &rows),
                Value::Array(all.iter().map(workspace_json).collect()),
            )
        }
        WorkspaceCommand::Show { name } => {
            let found = control.workspace(name).map_err(said)?;
            let engine = host.engine(&found)?;
            let view = engine.encryption().map_err(said)?;
            let items = engine.item_ids(Partition::Current).map_err(said)?.len();
            let plain = engine.item_ids(Partition::Plain).map_err(said)?.len();
            let (used, reserved) = (
                engine.used_bytes().map_err(said)?,
                engine.reserved_bytes().map_err(said)?,
            );
            let quota = host.config.limits_for(found.quota_bytes).quota_bytes;
            let keys = control.keys(Some(name)).map_err(said)?;
            let now = SystemClock.now();
            let mut human = format!(
                "workspace   {}\nid          {}\nitems       {items}{}\nused        {} of {} ({} promised to uploads)\nencryption  {}{}\n",
                found.name,
                found.id.as_str(),
                if plain > 0 {
                    format!(" (+{plain} set aside, plaintext)")
                } else {
                    String::new()
                },
                size(used),
                size(quota),
                size(reserved),
                state_text(view.state),
                view.key_id
                    .as_ref()
                    .map(|key| format!(", key {}", key.as_str()))
                    .unwrap_or_default(),
            );
            if let Some(session) = &view.rewrite {
                human.push_str(&format!(
                    "rewrite     {}; see `passalong-server rewrite show {}`\n",
                    rewrite_line(session, now),
                    found.name
                ));
            }
            human.push_str(&format!("keys        {}\n", keys.len()));
            for key in &keys {
                human.push_str(&format!(
                    "            {}  {}  {}\n",
                    key.id.as_str(),
                    key_state(key.state(now)),
                    key.label
                ));
            }
            let mut machine = workspace_json(&found);
            machine["items"] = json!(items);
            machine["plainItems"] = json!(plain);
            machine["usedBytes"] = json!(used.to_string());
            machine["reservedBytes"] = json!(reserved.to_string());
            machine["effectiveQuotaBytes"] = json!(quota.to_string());
            machine["encryption"] = json!({
                "state": state_text(view.state),
                "keyId": view.key_id.as_ref().map(|key| key.as_str().to_owned()),
                "rewrite": view.rewrite.as_ref().map(|session| rewrite_json(session, now)),
            });
            machine["keys"] = Value::Array(keys.iter().map(|key| key_json(key, now)).collect());
            host.print(human, machine)
        }
        WorkspaceCommand::Delete { name, yes, force } => {
            let found = control.workspace(name).map_err(said)?;
            let open = host.engine(&found)?.session_view().map_err(said)?.is_some();
            if open && !force {
                return Err(format!(
                    "workspace `{name}` has a rewrite session open; abort it first (`passalong-server rewrite abort {name}`), or delete with --force"
                ));
            }
            if !yes && !typed_again(name)? {
                return Err(format!(
                    "not deleted. Type the workspace's name to confirm, or pass --yes:\n  passalong-server workspace delete {name} --yes"
                ));
            }
            control.delete_workspace(name).map_err(said)?;
            // The rows are gone; a directory left by a kill here is found by `check`.
            let dir = host.config.workspace_dir(&found.id);
            if dir.exists() {
                std::fs::remove_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
            }
            host.print(
                format!("deleted workspace `{name}`, its items, and its keys\n"),
                json!({ "deleted": name }),
            )
        }
    }
}

/// Whether standard input, when someone is there, repeats the name.
fn typed_again(name: &str) -> Result<bool, String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("This deletes workspace `{name}` with every item and key. Type its name to confirm: ");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(said)?;
    Ok(line.trim() == name)
}

// ---------- keys ----------

fn key_state(state: KeyState) -> &'static str {
    match state {
        KeyState::Active => "active",
        KeyState::Expired => "expired",
        KeyState::Revoked => "revoked",
    }
}

fn role_text(role: Role) -> &'static str {
    match role {
        Role::ReadWrite => "read-write",
        Role::ReadOnly => "read-only",
    }
}

fn key_json(key: &KeyInfo, now: u64) -> Value {
    json!({
        "id": key.id.as_str(),
        "workspace": key.workspace_name,
        "label": key.label,
        "role": role_text(key.role),
        "state": key_state(key.state(now)),
        "createdAt": timestamp(key.created_at),
        "expiresAt": key.expires_at.map(timestamp),
        "revokedAt": key.revoked_at.map(timestamp),
        "lastUsedAt": key.last_used_at.map(timestamp),
    })
}

fn expiry(
    expires: Option<&str>,
    never: bool,
    default: Option<&str>,
) -> Result<Option<u64>, String> {
    if never {
        return Ok(None);
    }
    expires.or(default).map(parse_duration).transpose()
}

pub fn key(host: &Host, command: &KeyCommand) -> Done {
    let control = host.control()?;
    let now = SystemClock.now();
    match command {
        KeyCommand::Create {
            workspace,
            label,
            expires,
            never,
            read_only,
        } => {
            let lasts = expiry(expires.as_deref(), *never, Some(DEFAULT_EXPIRY))?;
            let role = if *read_only {
                Role::ReadOnly
            } else {
                Role::ReadWrite
            };
            let (key, info) = control
                .create_key(workspace, label, role, lasts)
                .map_err(said)?;
            let until = match (info.expires_at, expires) {
                (None, _) => "it never expires".to_owned(),
                (Some(at), Some(_)) => format!("it expires {}", timestamp(at)),
                (Some(at), None) => format!(
                    "it expires in 90 days, {}; --expires or --never chooses otherwise",
                    timestamp(at)
                ),
            };
            // The one place a secret leaves the process.
            let token = key.reveal();
            if host.json {
                let mut machine = key_json(&info, now);
                machine["key"] = json!(token);
                return Ok(pretty(&machine));
            }
            Ok(format!(
                "{token}\n\nThis is the key for `{label}` on workspace `{workspace}` ({}); {until}.\nIt is shown this once and cannot be shown again: only its hash is kept.\nOn the device it belongs in .env as PASSALONG_API_KEY. If it is lost: create another, and\n  passalong-server key revoke {}\n",
                role_text(role),
                info.id.as_str()
            ))
        }
        KeyCommand::List { workspace } => {
            let keys = control.keys(workspace.as_deref()).map_err(said)?;
            let rows: Vec<Vec<String>> = keys
                .iter()
                .map(|key| {
                    vec![
                        key.id.as_str().to_owned(),
                        key.workspace_name.clone(),
                        key.label.clone(),
                        role_text(key.role).to_owned(),
                        key_state(key.state(now)).to_owned(),
                        when(key.expires_at, "never"),
                        when(key.last_used_at, "-"),
                    ]
                })
                .collect();
            host.print(
                table(
                    &[
                        "ID",
                        "WORKSPACE",
                        "LABEL",
                        "ROLE",
                        "STATE",
                        "EXPIRES",
                        "LAST USED",
                    ],
                    &rows,
                ),
                Value::Array(keys.iter().map(|key| key_json(key, now)).collect()),
            )
        }
        KeyCommand::Extend { id, expires, never } => {
            let lasts = expiry(expires.as_deref(), *never, None)?;
            let key = control.extend_key(id, lasts).map_err(said)?;
            let note = if key.revoked_at.is_some() {
                " It was revoked, and stays revoked."
            } else {
                ""
            };
            host.print(
                format!(
                    "key {id} now expires: {}.{note}\n",
                    when(key.expires_at, "never")
                ),
                key_json(&key, now),
            )
        }
        KeyCommand::Revoke { id } => {
            let key = control.revoke_key(id).map_err(said)?;
            host.print(
                format!(
                    "key {id} (`{}`) is revoked: it is refused from the next request on\n",
                    key.label
                ),
                key_json(&key, now),
            )
        }
        KeyCommand::Delete { id } => {
            let key = control.delete_key(id).map_err(said)?;
            host.print(
                format!("key {id} (`{}`) is deleted\n", key.label),
                json!({ "deleted": id }),
            )
        }
        KeyCommand::Prune { older_than } => {
            let pruned = control
                .prune_keys(parse_duration(older_than)?)
                .map_err(said)?;
            host.print(
                format!(
                    "pruned {pruned} keys that expired or were revoked more than {older_than} ago\n"
                ),
                json!({ "pruned": pruned }),
            )
        }
    }
}

// ---------- rewrite ----------

fn kind_text(kind: RewriteKind) -> &'static str {
    match kind {
        RewriteKind::Migrate => "migrate",
        RewriteKind::Rotate => "rotate",
    }
}

fn rewrite_line(session: &passalong_server_core::workspace::SessionView, now: u64) -> String {
    let lease = if session.lease_expires_at > now {
        format!(
            "its lease runs until {}",
            timestamp(session.lease_expires_at)
        )
    } else {
        format!("its lease ended {}", timestamp(session.lease_expires_at))
    };
    format!(
        "{} by key {}, {} of {} items staged, {lease}",
        kind_text(session.kind),
        session.holder.as_str(),
        session.staged_ids.len(),
        session.source_items
    )
}

fn rewrite_json(session: &passalong_server_core::workspace::SessionView, now: u64) -> Value {
    json!({
        "kind": kind_text(session.kind),
        "holder": session.holder.as_str(),
        "leaseExpiresAt": timestamp(session.lease_expires_at),
        "leaseEnded": session.lease_expires_at <= now,
        "newKeyId": session.new_key_id.as_str(),
        "staged": session.staged_ids.len(),
        "sourceItems": session.source_items,
    })
}

pub fn rewrite(host: &Host, command: &RewriteCommand) -> Done {
    let control = host.control()?;
    let now = SystemClock.now();
    let (RewriteCommand::Show { workspace } | RewriteCommand::Abort { workspace, .. }) = command;
    let found = control.workspace(workspace).map_err(said)?;
    let engine = host.engine(&found)?;
    let Some(session) = engine.session_view().map_err(said)? else {
        return Err(format!(
            "workspace `{workspace}` has no rewrite session open"
        ));
    };
    match command {
        RewriteCommand::Show { .. } => host.print(
            format!(
                "{}\nUntil a device finishes or aborts it (`passalong encrypt --recover`), or you abort it here, every writer is refused.\n",
                rewrite_line(&session, now)
            ),
            rewrite_json(&session, now),
        ),
        RewriteCommand::Abort { force, .. } => match engine.operator_abort_rewrite(*force) {
            Ok(view) => host.print(
                format!("aborted. Workspace `{workspace}` is {} again, as before the rewrite; what was staged is dropped.\n", state_text(view.state)),
                json!({ "aborted": workspace, "state": state_text(view.state) }),
            ),
            Err(ApiError::LeaseHeld) => Err(format!(
                "its holder's lease runs until {}: the device may still be at work. Wait, or abort with --force",
                timestamp(session.lease_expires_at)
            )),
            Err(err) => Err(said(err)),
        },
    }
}

// ---------- check ----------

pub fn check(host: &Host, config_file: &Path) -> Done {
    let mut lines = vec![format!("ok    configuration {}", config_file.display())];
    let mut failed = 0;
    let mut step = |ok: bool, text: String| {
        failed += usize::from(!ok);
        lines.push(format!("{}  {text}", if ok { "ok  " } else { "FAIL" }));
    };
    let data = &host.config.server.data_dir;
    match std::fs::metadata(data) {
        Ok(meta) => {
            let mode = meta.permissions().mode() & 0o777;
            step(
                mode == 0o700,
                format!(
                    "data directory {} (mode {mode:o}, should be 700)",
                    data.display()
                ),
            );
        }
        Err(err) => step(false, format!("data directory {}: {err}", data.display())),
    }
    match host
        .control()
        .and_then(|control| control.workspaces().map_err(said))
    {
        Err(err) => step(false, format!("control database: {err}")),
        Ok(workspaces) => {
            step(
                true,
                format!(
                    "control database, schema {SCHEMA_VERSION}, {} workspace{}",
                    workspaces.len(),
                    if workspaces.len() == 1 { "" } else { "s" }
                ),
            );
            for workspace in &workspaces {
                let opened = host.engine(workspace).and_then(|engine| {
                    let items = engine.item_ids(Partition::Current).map_err(said)?.len();
                    let open = engine.session_view().map_err(said)?.is_some();
                    Ok((items, open))
                });
                match opened {
                    Ok((items, open)) => step(
                        true,
                        format!(
                            "workspace `{}`: {items} items{}",
                            workspace.name,
                            if open {
                                "; a rewrite session is open"
                            } else {
                                ""
                            }
                        ),
                    ),
                    Err(err) => step(false, err),
                }
            }
            let known: Vec<&str> = workspaces
                .iter()
                .map(|workspace| workspace.id.as_str())
                .collect();
            for entry in std::fs::read_dir(data.join("workspaces"))
                .into_iter()
                .flatten()
                .flatten()
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !known.contains(&name.as_str()) {
                    step(
                        false,
                        format!(
                            "{} belongs to no workspace; a deletion was cut short. Remove it",
                            entry.path().display()
                        ),
                    );
                }
            }
        }
    }
    if host.config.listen.mode == ListenMode::Tls {
        match &host.config.tls {
            None => lines.push("note  listen.mode is \"tls\" and there is no [tls] section yet; `serve` will need one".to_owned()),
            Some(tls) => {
                for file in [&tls.cert_file, &tls.key_file] {
                    if !file.is_file() {
                        lines.push(format!("note  {} is not there yet; `serve` will need it", file.display()));
                    }
                }
            }
        }
    }
    let text = lines.join("\n") + "\n";
    if failed == 0 {
        Ok(text)
    } else {
        Err(format!("{text}{failed} checks failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Vars(BTreeMap<&'static str, &'static str>);

    impl config::Environment for Vars {
        fn var(&self, name: &str) -> Option<String> {
            self.0.get(name).map(|value| (*value).to_owned())
        }
        fn is_file(&self, _: &Path) -> bool {
            false
        }
    }

    fn paths(vars: &[(&'static str, &'static str)], root: bool) -> (String, String) {
        let (file, data) = default_paths(&Vars(vars.iter().copied().collect()), root);
        (file.display().to_string(), data.display().to_string())
    }

    #[test]
    fn init_writes_where_later_commands_will_look() {
        let system = (
            "/etc/passalong-server/config.toml".to_owned(),
            "/var/lib/passalong-server".to_owned(),
        );
        assert_eq!(paths(&[], false), system);
        assert_eq!(paths(&[("HOME", "/root")], true), system);
        assert_eq!(paths(&[("HOME", "")], false), system);
        assert_eq!(
            paths(&[("HOME", "/home/me")], false),
            (
                "/home/me/.config/passalong-server/config.toml".to_owned(),
                "/home/me/.local/share/passalong-server".to_owned()
            )
        );
        assert_eq!(
            paths(&[("HOME", "/home/me"), ("XDG_CONFIG_HOME", "/x")], false).0,
            "/x/passalong-server/config.toml"
        );
        // The container: a home, and a variable that says otherwise.
        let named = [
            ("HOME", "/var/lib/passalong-server"),
            (
                "PASSALONG_SERVER_CONFIG_FILE",
                "/etc/passalong-server/config.toml",
            ),
        ];
        assert_eq!(paths(&named, false).0, "/etc/passalong-server/config.toml");
        // Set and empty is unset.
        assert_eq!(
            paths(
                &[("HOME", "/home/me"), ("PASSALONG_SERVER_CONFIG_FILE", "")],
                false
            )
            .0,
            "/home/me/.config/passalong-server/config.toml"
        );
    }
}
