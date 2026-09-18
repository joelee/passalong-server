//! `config.toml`: where it is found, what it may hold, and what is refused.
//!
//! Unknown keys are refused, so a typing error is an error and not a
//! silently ignored wish. No secret belongs here: the server has none of its
//! own, API keys are made by `key create` and kept as hashes, and the TLS
//! private key is a file this one only names.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::workspace::Limits;

/// `config.sample.toml`, which `init` writes with the data directory filled
/// in, and which a test keeps valid.
pub const SAMPLE: &str = include_str!("../../../config.sample.toml");

const APP: &str = "passalong-server";
const FILE_VARIABLE: &str = "PASSALONG_SERVER_CONFIG_FILE";
const LEVEL_VARIABLE: &str = "PASSALONG_SERVER_LOG_LEVEL";

/// What the configuration needs of the world, so that tests can supply it.
pub trait Environment {
    /// An environment variable; empty counts as unset.
    fn var(&self, name: &str) -> Option<String>;
    /// Whether `path` is a file.
    fn is_file(&self, path: &Path) -> bool;
}

/// The process's environment and the real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct Process;

impl Environment for Process {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}

/// Why there is no configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// The five levels `AGENTS.md` names, as the client has them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Failures only.
    Error,
    /// And what deserves a look.
    Warning,
    /// And what happened.
    #[default]
    Info,
    /// And how.
    Verbose,
    /// And everything.
    Debug,
}

impl LogLevel {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "error" => Self::Error,
            "warning" => Self::Warning,
            "info" => Self::Info,
            "verbose" => Self::Verbose,
            "debug" => Self::Debug,
            _ => return None,
        })
    }
}

/// How the server listens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ListenMode {
    /// HTTPS, with the files under `[tls]`.
    #[default]
    Tls,
    /// Plain HTTP, for a reverse proxy that terminates TLS.
    Plain,
}

/// `[server]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Server {
    /// `server.log_level`.
    pub log_level: LogLevel,
    /// `server.data_dir`.
    pub data_dir: PathBuf,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            log_level: LogLevel::default(),
            data_dir: PathBuf::from("/var/lib/passalong-server"),
        }
    }
}

/// `[listen]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Listen {
    /// `listen.address`.
    pub address: SocketAddr,
    /// `listen.mode`.
    pub mode: ListenMode,
    /// `listen.behind_proxy`.
    pub behind_proxy: bool,
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            address: SocketAddr::from(([0, 0, 0, 0], 8443)),
            mode: ListenMode::default(),
            behind_proxy: false,
        }
    }
}

/// `[tls]`: both files, or no section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    /// `tls.cert_file`.
    pub cert_file: PathBuf,
    /// `tls.key_file`.
    pub key_file: PathBuf,
}

/// `[limits]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsSection {
    /// `limits.max_item_bytes`; `None` is `"unlimited"`.
    #[serde(deserialize_with = "size_or_unlimited")]
    pub max_item_bytes: Option<u64>,
    /// `limits.workspace_quota_bytes`.
    #[serde(deserialize_with = "size")]
    pub workspace_quota_bytes: u64,
    /// `limits.auth_failures_per_minute`.
    pub auth_failures_per_minute: u32,
}

impl Default for LimitsSection {
    fn default() -> Self {
        Self {
            max_item_bytes: None,
            workspace_quota_bytes: 20 << 30,
            auth_failures_per_minute: 10,
        }
    }
}

/// `[rewrite]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Rewrite {
    /// `rewrite.lease_secs`.
    pub lease_secs: u64,
}

impl Default for Rewrite {
    fn default() -> Self {
        Self { lease_secs: 600 }
    }
}

/// `[staging]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Staging {
    /// `staging.max_age_hours`.
    pub max_age_hours: u64,
}

impl Default for Staging {
    fn default() -> Self {
        Self { max_age_hours: 24 }
    }
}

/// The whole file. `docs/configuration.md` documents every key.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// `[server]`.
    pub server: Server,
    /// `[listen]`.
    pub listen: Listen,
    /// `[tls]`.
    pub tls: Option<Tls>,
    /// `[limits]`.
    pub limits: LimitsSection,
    /// `[rewrite]`.
    pub rewrite: Rewrite,
    /// `[staging]`.
    pub staging: Staging,
}

impl Config {
    /// Where the control database is.
    pub fn control_database(&self) -> PathBuf {
        self.server.data_dir.join("control.sqlite")
    }

    /// Where a workspace's items are.
    pub fn workspace_dir(&self, workspace: &crate::ids::WorkspaceId) -> PathBuf {
        self.server
            .data_dir
            .join("workspaces")
            .join(workspace.as_str())
    }

    /// The limits of a workspace whose own quota is `quota_bytes`; `None`
    /// is the configured default.
    pub fn limits_for(&self, quota_bytes: Option<u64>) -> Limits {
        Limits {
            quota_bytes: quota_bytes.unwrap_or(self.limits.workspace_quota_bytes),
            max_item_bytes: self.limits.max_item_bytes,
            staging_secs: self.staging.max_age_hours.saturating_mul(3_600),
            lease_secs: self.rewrite.lease_secs,
        }
    }
}

/// A size: a number of bytes, alone or with `KiB`, `MiB`, `GiB`, or `TiB`;
/// or `unlimited`, which is `None`.
///
/// # Errors
///
/// A message saying what a size looks like.
pub fn parse_size(text: &str) -> Result<Option<u64>, String> {
    if text == "unlimited" {
        return Ok(None);
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let (number, unit) = text.split_at(digits);
    let shift = match unit.trim_start_matches(' ') {
        "" => 0,
        "KiB" => 10,
        "MiB" => 20,
        "GiB" => 30,
        "TiB" => 40,
        _ => {
            return Err(format!(
                "`{text}` is not a size: write a number of bytes, or one with KiB, MiB, GiB, or TiB, such as \"20 GiB\""
            ));
        }
    };
    let spaces = unit.len() - unit.trim_start_matches(' ').len();
    let number: u64 = number
        .parse()
        .ok()
        .filter(|_| spaces <= 1)
        .ok_or_else(|| format!("`{text}` is not a size: it must start with a whole number"))?;
    number
        .checked_mul(1 << shift)
        .map(Some)
        .ok_or_else(|| format!("`{text}` is more bytes than can be counted"))
}

/// A duration for the command line: a whole number and `s`, `m`, `h`, `d`,
/// or `w`, such as `90d`. In seconds.
///
/// # Errors
///
/// A message saying what a duration looks like.
pub fn parse_duration(text: &str) -> Result<u64, String> {
    let bad = || {
        format!(
            "`{text}` is not a duration: write a whole number and s, m, h, d, or w, such as 90d"
        )
    };
    let unit = text.chars().last().ok_or_else(bad)?;
    let each = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        'w' => 7 * 86_400,
        _ => return Err(bad()),
    };
    let number = &text[..text.len() - 1];
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(each))
        .ok_or_else(bad)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SizeText {
    Number(u64),
    Text(String),
}

fn size_or_unlimited<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<u64>, D::Error> {
    match SizeText::deserialize(de)? {
        SizeText::Number(bytes) => Ok(Some(bytes)),
        SizeText::Text(text) => parse_size(&text).map_err(serde::de::Error::custom),
    }
}

fn size<'de, D: serde::Deserializer<'de>>(de: D) -> Result<u64, D::Error> {
    size_or_unlimited(de)?
        .ok_or_else(|| serde::de::Error::custom("a quota cannot be \"unlimited\": the disk is not"))
}

/// Where the configuration file is: `explicit`, which is `--config`, or the
/// first that exists of the places `AGENTS.md` lists.
///
/// # Errors
///
/// When a file that was named does not exist, or none is found anywhere; the
/// message lists where it looked.
pub fn find(explicit: Option<&Path>, env: &dyn Environment) -> Result<PathBuf, ConfigError> {
    let set = |name: &str| env.var(name).filter(|value| !value.is_empty());
    let named = explicit
        .map(|path| ("--config", path.to_path_buf()))
        .or_else(|| set(FILE_VARIABLE).map(|path| (FILE_VARIABLE, PathBuf::from(path))));
    if let Some((by, path)) = named {
        return if env.is_file(&path) {
            Ok(path)
        } else {
            Err(ConfigError(format!(
                "{by} names {}, which is not a file",
                path.display()
            )))
        };
    }
    let mut places = Vec::new();
    if let Some(xdg) = set("XDG_CONFIG_HOME") {
        places.push(PathBuf::from(xdg).join(APP).join("config.toml"));
    }
    if let Some(home) = set("HOME") {
        places.push(
            PathBuf::from(home)
                .join(".config")
                .join(APP)
                .join("config.toml"),
        );
    }
    places.push(PathBuf::from("/etc").join(APP).join("config.toml"));
    places.push(PathBuf::from("./config.toml"));
    if let Some(found) = places.iter().find(|path| env.is_file(path)) {
        return Ok(found.clone());
    }
    let looked: Vec<String> = places
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    Err(ConfigError(format!(
        "no configuration file; looked at {}. `passalong-server init` writes one",
        looked.join(", ")
    )))
}

/// Parses and validates the text of the file at `path`.
///
/// # Errors
///
/// What is wrong, with the file's name; for an unknown key, the key and the
/// line.
pub fn parse(text: &str, path: &Path, env: &dyn Environment) -> Result<Config, ConfigError> {
    let mut config: Config =
        toml::from_str(text).map_err(|err| ConfigError(format!("{}: {err}", path.display())))?;
    if let Some(level) = env.var(LEVEL_VARIABLE).filter(|value| !value.is_empty()) {
        config.server.log_level = LogLevel::parse(&level).ok_or_else(|| {
            ConfigError(format!(
                "{LEVEL_VARIABLE} is `{level}`; it must be error, warning, info, verbose, or debug"
            ))
        })?;
    }
    let public = !config.listen.address.ip().is_loopback();
    if config.listen.mode == ListenMode::Plain && public && !config.listen.behind_proxy {
        return Err(ConfigError(format!(
            "{}: listen.mode is \"plain\" on {}, which is not a loopback address. Plain HTTP would carry API keys in the clear. If a TLS-terminating proxy is in front, say so with listen.behind_proxy = true",
            path.display(),
            config.listen.address
        )));
    }
    Ok(config)
}

/// Finds, reads, and parses the configuration.
///
/// # Errors
///
/// As [`find`] and [`parse`], and when the file cannot be read.
pub fn load(
    explicit: Option<&Path>,
    env: &dyn Environment,
) -> Result<(Config, PathBuf), ConfigError> {
    let path = find(explicit, env)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|err| ConfigError(format!("{}: {err}", path.display())))?;
    Ok((parse(&text, &path, env)?, path))
}

/// The file `init` writes: the sample, with `data_dir` filled in.
pub fn initial_file(data_dir: &Path) -> String {
    SAMPLE
        .lines()
        .map(|line| {
            if line.starts_with("data_dir = ") {
                format!("data_dir = {:?}", data_dir.display().to_string())
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    /// An environment and a filesystem made of two maps.
    #[derive(Default)]
    struct Fake {
        vars: BTreeMap<&'static str, &'static str>,
        files: Vec<&'static str>,
    }

    impl Environment for Fake {
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).map(|value| (*value).to_owned())
        }
        fn is_file(&self, path: &Path) -> bool {
            self.files.iter().any(|file| Path::new(file) == path)
        }
    }

    #[test]
    fn the_first_place_that_has_a_file_wins() {
        let everywhere = Fake {
            vars: [
                ("PASSALONG_SERVER_CONFIG_FILE", "/env/config.toml"),
                ("XDG_CONFIG_HOME", "/xdg"),
                ("HOME", "/home/op"),
            ]
            .into(),
            files: vec![
                "/given.toml",
                "/env/config.toml",
                "/xdg/passalong-server/config.toml",
                "/home/op/.config/passalong-server/config.toml",
                "/etc/passalong-server/config.toml",
                "./config.toml",
            ],
        };
        let found = |env: &Fake, explicit: Option<&str>| {
            find(explicit.map(Path::new), env).map(|path| path.display().to_string())
        };
        assert_eq!(
            found(&everywhere, Some("/given.toml")).unwrap(),
            "/given.toml"
        );
        let missing = found(&everywhere, Some("/missing.toml")).unwrap_err();
        assert!(missing.to_string().contains("--config"), "{missing}");
        let mut env = everywhere;
        for expected in [
            "/env/config.toml",
            "/xdg/passalong-server/config.toml",
            "/home/op/.config/passalong-server/config.toml",
            "/etc/passalong-server/config.toml",
            "./config.toml",
        ] {
            assert_eq!(found(&env, None).unwrap(), expected);
            env.files.retain(|file| *file != expected);
            if expected.starts_with("/env") {
                env.vars.remove("PASSALONG_SERVER_CONFIG_FILE");
            }
        }
        let err = found(&env, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("/etc/passalong-server/config.toml"),
            "{err}"
        );
        // A file the environment names must exist: silence would be a trap.
        env.vars
            .insert("PASSALONG_SERVER_CONFIG_FILE", "/nowhere.toml");
        assert!(
            found(&env, None)
                .unwrap_err()
                .to_string()
                .contains("/nowhere.toml")
        );
    }

    #[test]
    fn an_empty_file_is_all_defaults() {
        let config = parse("", Path::new("/c.toml"), &Fake::default()).unwrap();
        assert_eq!(config.server.log_level, LogLevel::Info);
        assert_eq!(
            config.server.data_dir,
            Path::new("/var/lib/passalong-server")
        );
        assert_eq!(config.listen.address.to_string(), "0.0.0.0:8443");
        assert_eq!(config.listen.mode, ListenMode::Tls);
        assert!(!config.listen.behind_proxy);
        assert_eq!(config.limits.max_item_bytes, None);
        assert_eq!(config.limits.workspace_quota_bytes, 20 * 1024 * 1024 * 1024);
        assert_eq!(config.limits.auth_failures_per_minute, 10);
        assert_eq!(config.rewrite.lease_secs, 600);
        assert_eq!(config.staging.max_age_hours, 24);
        assert_eq!(
            config.control_database(),
            Path::new("/var/lib/passalong-server/control.sqlite")
        );

        let limits = config.limits_for(Some(5));
        assert_eq!(
            (
                limits.quota_bytes,
                limits.max_item_bytes,
                limits.staging_secs,
                limits.lease_secs
            ),
            (5, None, 86_400, 600)
        );
        assert_eq!(config.limits_for(None).quota_bytes, 20 * 1024 * 1024 * 1024);
    }

    #[test]
    fn the_sample_file_is_a_valid_file() {
        let config = parse(SAMPLE, Path::new("config.sample.toml"), &Fake::default()).unwrap();
        assert_eq!(config.limits.max_item_bytes, None);
        assert_eq!(
            config.tls.as_ref().unwrap().cert_file,
            Path::new("/etc/passalong-server/tls/cert.pem")
        );
    }

    #[test]
    fn what_is_not_a_known_key_is_refused_by_name() {
        for (text, culprit) in [
            ("[server]\nlog_levle = \"info\"\n", "log_levle"),
            ("[sever]\n", "sever"),
            ("[limits]\nmax_item_bytes = \"2 GiB\"\nquota = 3\n", "quota"),
        ] {
            let err = parse(text, Path::new("/etc/c.toml"), &Fake::default())
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(culprit) && err.contains("/etc/c.toml"),
                "{err}"
            );
        }
    }

    #[test]
    fn sizes_and_levels_are_parsed_strictly() {
        let ok = |text: &str| parse_size(text).unwrap();
        assert_eq!(ok("0"), Some(0));
        assert_eq!(ok("512"), Some(512));
        assert_eq!(ok("1 KiB"), Some(1024));
        assert_eq!(ok("20GiB"), Some(20 << 30));
        assert_eq!(ok("3 MiB"), Some(3 << 20));
        assert_eq!(ok("2 TiB"), Some(2 << 40));
        assert_eq!(ok("unlimited"), None);
        for bad in [
            "",
            "GiB",
            "1 GB",
            "-1",
            "1.5 GiB",
            "1 gib",
            "99999999999 TiB",
            "un limited",
        ] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
        let err = parse(
            "[limits]\nworkspace_quota_bytes = \"unlimited\"\n",
            Path::new("/c.toml"),
            &Fake::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("workspace_quota_bytes"), "{err}");
        let err = parse(
            "[server]\nlog_level = \"loud\"\n",
            Path::new("/c.toml"),
            &Fake::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("loud"), "{err}");
    }

    #[test]
    fn durations_are_a_number_and_a_unit() {
        assert_eq!(parse_duration("90d").unwrap(), 90 * 86_400);
        assert_eq!(parse_duration("12h").unwrap(), 12 * 3_600);
        assert_eq!(parse_duration("30m").unwrap(), 1_800);
        assert_eq!(parse_duration("45s").unwrap(), 45);
        assert_eq!(parse_duration("2w").unwrap(), 14 * 86_400);
        for bad in [
            "",
            "d",
            "90",
            "90 d",
            "-1d",
            "1.5h",
            "1y",
            "99999999999999999999d",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_environment_overrides_the_log_level_and_nothing_else() {
        let env = Fake {
            vars: [("PASSALONG_SERVER_LOG_LEVEL", "debug")].into(),
            files: vec![],
        };
        let config = parse(
            "[server]\nlog_level = \"error\"\n",
            Path::new("/c.toml"),
            &env,
        )
        .unwrap();
        assert_eq!(config.server.log_level, LogLevel::Debug);
        let env = Fake {
            vars: [("PASSALONG_SERVER_LOG_LEVEL", "")].into(),
            files: vec![],
        };
        assert_eq!(
            parse("", Path::new("/c.toml"), &env)
                .unwrap()
                .server
                .log_level,
            LogLevel::Info
        );
        let env = Fake {
            vars: [("PASSALONG_SERVER_LOG_LEVEL", "shouting")].into(),
            files: vec![],
        };
        assert!(
            parse("", Path::new("/c.toml"), &env)
                .unwrap_err()
                .to_string()
                .contains("PASSALONG_SERVER_LOG_LEVEL")
        );
    }

    #[test]
    fn plain_http_on_a_public_address_must_be_meant() {
        let plain = "[listen]\nmode = \"plain\"\naddress = \"0.0.0.0:8080\"\n";
        let err = parse(plain, Path::new("/c.toml"), &Fake::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("behind_proxy"), "{err}");
        parse(
            &format!("{plain}behind_proxy = true\n"),
            Path::new("/c.toml"),
            &Fake::default(),
        )
        .unwrap();
        parse(
            "[listen]\nmode = \"plain\"\naddress = \"127.0.0.1:8080\"\n",
            Path::new("/c.toml"),
            &Fake::default(),
        )
        .unwrap();
        parse(
            "[listen]\nmode = \"plain\"\naddress = \"[::1]:8080\"\n",
            Path::new("/c.toml"),
            &Fake::default(),
        )
        .unwrap();
        // TLS needs its two files named.
        let err = parse(
            "[listen]\nmode = \"tls\"\n[tls]\ncert_file = \"/c.pem\"\n",
            Path::new("/c.toml"),
            &Fake::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("key_file"), "{err}");
    }

    #[test]
    fn init_writes_a_file_that_names_the_data_directory_it_was_given() {
        let text = initial_file(Path::new("/srv/pass"));
        let config = parse(&text, Path::new("/c.toml"), &Fake::default()).unwrap();
        assert_eq!(config.server.data_dir, Path::new("/srv/pass"));
        assert!(
            text.lines().filter(|line| line.starts_with('#')).count() > 10,
            "it explains itself"
        );
    }
}
