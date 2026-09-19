//! `tls letsencrypt`: how to get, install, and renew a Let's Encrypt
//! certificate for this server, written out for this host's configuration.
//! It prints and changes nothing: `certbot` does the work, and already does
//! it well. What no generic guide says is what is particular to this server:
//! where the pair goes and whose it must be, that no restart is needed, and
//! that a renewal with a new key locks out every device that pinned the old.

use std::fmt::Write as _;
use std::path::PathBuf;

/// Where `certbot` runs the hooks of every renewal from.
pub const HOOK_PATH: &str = "/etc/letsencrypt/renewal-hooks/deploy/passalong-server";

/// What the walkthrough is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    /// Host names, the first of which names the certificate's directory.
    pub names: Vec<String>,
    pub email: Option<String>,
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    /// `uid:gid` of the data directory's owner, whom the server runs as.
    pub owner: String,
    /// The pair lives in the `config` volume of `deploy/docker`.
    pub docker: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walkthrough {
    pub certbot: String,
    pub hook: String,
    /// Installs the certificate that was just issued: `certbot` runs the
    /// hooks of that directory at renewals, not at the first issuance.
    pub first_run: String,
    pub text: String,
}

/// A name a public authority can certify: a DNS name with a dot in it.
/// Let's Encrypt does not certify addresses or single labels.
fn certifiable(name: &str) -> bool {
    name.parse::<std::net::IpAddr>().is_err()
        && name.contains('.')
        && name.len() <= 253
        && name.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Single-quoted for `sh`, whatever is in it.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn hook(inputs: &Inputs) -> String {
    let lineage = &inputs.names[0];
    let cert = quoted(&inputs.cert_file.display().to_string());
    let key = quoted(&inputs.key_file.display().to_string());
    let head = format!(
        "#!/bin/sh\n# {HOOK_PATH}\n# Written from `passalong-server tls letsencrypt`. certbot runs it after every\n# renewal, with RENEWED_LINEAGE naming the renewed certificate's directory.\nset -eu\n\n# Other certificates of this machine are none of this hook's business.\n[ \"${{RENEWED_LINEAGE##*/}}\" = {} ] || exit 0\n\nCERT={cert}\nKEY={key}\n\n# A whole pair or nothing: never half of a renewal over a pair that works.\nfor part in fullchain.pem privkey.pem; do\n    if [ ! -s \"$RENEWED_LINEAGE/$part\" ]; then\n        echo \"passalong-server: $RENEWED_LINEAGE/$part is missing or empty; the pair in use is kept\" >&2\n        exit 1\n    fi\ndone\n\n",
        quoted(lineage)
    );
    let install = if inputs.docker {
        "# The directory of deploy/docker's compose.yaml: set it.\nCOMPOSE_DIR=/path/to/passalong-server/deploy/docker\ncd \"$COMPOSE_DIR\"\n\n# Through `exec`, as the server's user: what `docker compose cp` puts there\n# belongs to root, and the server could not read its key.\ninto() { docker compose exec -T server sh -c \"umask $1; cat > \\\"\\$0\\\"\" \"$2\"; }\ninto 077 \"$KEY.new\" < \"$RENEWED_LINEAGE/privkey.pem\"\ninto 022 \"$CERT.new\" < \"$RENEWED_LINEAGE/fullchain.pem\"\ndocker compose exec -T server sh -c 'mv \"$0.new\" \"$0\" && mv \"$1.new\" \"$1\"' \"$KEY\" \"$CERT\"\n".to_owned()
    } else {
        format!(
            "# The server's user, as uid:gid. The key is for its eyes only.\nOWNER={}\n\n# Written aside and moved into place, so the server never reads half a file.\ninstall -m 0600 -o \"${{OWNER%%:*}}\" -g \"${{OWNER##*:}}\" \"$RENEWED_LINEAGE/privkey.pem\" \"$KEY.new\"\ninstall -m 0644 -o \"${{OWNER%%:*}}\" -g \"${{OWNER##*:}}\" \"$RENEWED_LINEAGE/fullchain.pem\" \"$CERT.new\"\nmv \"$KEY.new\" \"$KEY\"\nmv \"$CERT.new\" \"$CERT\"\n",
            quoted(&inputs.owner)
        )
    };
    format!(
        "{head}{install}\n# No restart: the server looks at the pair every half minute.\necho \"passalong-server: installed the renewed pair of $RENEWED_LINEAGE\"\n"
    )
}

/// # Errors
///
/// When there is no name, or one that no public authority certifies.
pub fn walkthrough(inputs: &Inputs) -> Result<Walkthrough, String> {
    if inputs.names.is_empty() {
        return Err("give the name devices reach the server by: --host <name>".to_owned());
    }
    if let Some(odd) = inputs.names.iter().find(|name| !certifiable(name)) {
        return Err(format!(
            "{odd:?} is not a name Let's Encrypt certifies: it needs a public DNS name, not an address or a name without a dot. For those, `passalong-server tls self-signed` and a pin"
        ));
    }
    let lineage = &inputs.names[0];
    let mut certbot = String::from("sudo certbot certonly --standalone --reuse-key");
    match &inputs.email {
        Some(email) => {
            let _ = write!(certbot, " --agree-tos -m {}", quoted(email));
        }
        None => certbot.push_str(" --agree-tos --register-unsafely-without-email"),
    }
    for name in &inputs.names {
        let _ = write!(certbot, " -d {name}");
    }
    let first_run = format!("sudo RENEWED_LINEAGE=/etc/letsencrypt/live/{lineage} {HOOK_PATH}");
    let hook = hook(inputs);

    let mut text = String::new();
    let _ = write!(
        text,
        "Let's Encrypt for {names}. Nothing was changed; this is what to do.\n\n\
         1. Get the certificate. Port 80 of {lineage} must reach this machine while it runs\n   (`--webroot` or a DNS plugin instead of `--standalone` if it cannot):\n\n     {certbot}\n\n   \
         --reuse-key matters here: see 4.\n\n\
         2. Save the hook below as {HOOK_PATH}\n   and make it executable (`sudo chmod 755`).{docker} It installs every renewed pair at\n     {cert}\n     {key}\n   \
         for the server's user, the key 0600, a whole pair or nothing.\n\n\
         3. Run it once by hand, since certbot runs such hooks at renewals and not at the\n   first issuance:\n\n     {first_run}\n\n   \
         The server reads a changed pair within half a minute. No restart, now or at\n   any renewal; `sudo certbot renew --dry-run` rehearses one.\n\n\
         4. On the devices: a certificate from Let's Encrypt is trusted as it is, so they\n   need NO `tls_pin`; remove it where it is set. A device that pins all the same\n   pins the key, and certbot makes a new key at every renewal unless told\n   --reuse-key: without it, every pinned device is locked out some sixty days\n   from now. `passalong-server tls fingerprint` prints the pin of the pair in use.\n\n\
         ---- {HOOK_PATH} ----\n{hook}",
        names = inputs.names.join(", "),
        docker = if inputs.docker {
            "\n   Set COMPOSE_DIR in it. It runs on the host, where certbot runs."
        } else {
            ""
        },
        cert = inputs.cert_file.display(),
        key = inputs.key_file.display(),
    );
    Ok(Walkthrough {
        certbot,
        hook,
        first_run,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;
    use std::process::Command;

    fn inputs(dir: &Path) -> Inputs {
        let meta = std::fs::metadata(dir).unwrap();
        Inputs {
            names: vec!["nas.example".to_owned(), "pass.example.net".to_owned()],
            email: Some("me@example.net".to_owned()),
            cert_file: dir.join("tls/cert.pem"),
            key_file: dir.join("tls/key.pem"),
            owner: format!("{}:{}", meta.uid(), meta.gid()),
            docker: false,
        }
    }

    #[test]
    fn the_walkthrough_is_made_from_this_hosts_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let made = walkthrough(&inputs(dir.path())).unwrap();
        assert_eq!(
            made.certbot,
            "sudo certbot certonly --standalone --reuse-key --agree-tos -m 'me@example.net' -d nas.example -d pass.example.net"
        );
        assert_eq!(
            made.first_run,
            format!("sudo RENEWED_LINEAGE=/etc/letsencrypt/live/nas.example {HOOK_PATH}")
        );
        let cert = dir.path().join("tls/cert.pem").display().to_string();
        for part in [
            made.certbot.as_str(),
            made.first_run.as_str(),
            made.hook.as_str(),
            cert.as_str(),
            "NO `tls_pin`",
            "--reuse-key",
            "No restart",
            "Nothing was changed",
        ] {
            assert!(made.text.contains(part), "no {part:?} in:\n{}", made.text);
        }
        // Without an address to write to, certbot is told so, not left to ask.
        let anonymous = Inputs {
            email: None,
            ..inputs(dir.path())
        };
        assert!(
            walkthrough(&anonymous)
                .unwrap()
                .certbot
                .contains("--register-unsafely-without-email")
        );
    }

    #[test]
    fn only_names_a_public_authority_certifies() {
        let dir = tempfile::tempdir().unwrap();
        for odd in [
            "192.0.2.4",
            "::1",
            "nas",
            "nas..example",
            "-x.example",
            "a b.example",
            "",
        ] {
            let said = walkthrough(&Inputs {
                names: vec!["ok.example".to_owned(), odd.to_owned()],
                ..inputs(dir.path())
            })
            .unwrap_err();
            assert!(said.contains("tls self-signed"), "{odd:?}: {said}");
        }
        let none = Inputs {
            names: vec![],
            ..inputs(dir.path())
        };
        assert!(walkthrough(&none).unwrap_err().contains("--host"));
    }

    fn run_hook(hook: &str, lineage: &Path) -> std::process::Output {
        Command::new("sh")
            .args(["-c", hook])
            .env("RENEWED_LINEAGE", lineage)
            .output()
            .unwrap()
    }

    #[test]
    fn the_printed_hook_installs_a_whole_pair_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let tls = dir.path().join("tls");
        std::fs::create_dir(&tls).unwrap();
        std::fs::write(tls.join("cert.pem"), "old cert").unwrap();
        std::fs::write(tls.join("key.pem"), "old key").unwrap();
        let hook = walkthrough(&inputs(dir.path())).unwrap().hook;
        let lineage = dir.path().join("live/nas.example");
        std::fs::create_dir_all(&lineage).unwrap();
        let read = |name: &str| std::fs::read_to_string(tls.join(name)).unwrap();

        // Half a renewal: refused, and the pair in use is untouched.
        std::fs::write(lineage.join("fullchain.pem"), "new cert").unwrap();
        let refused = run_hook(&hook, &lineage);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("privkey.pem is missing"));
        assert_eq!(
            (read("cert.pem"), read("key.pem")),
            ("old cert".into(), "old key".into())
        );
        std::fs::write(lineage.join("privkey.pem"), "").unwrap();
        assert!(!run_hook(&hook, &lineage).status.success());
        assert_eq!(read("key.pem"), "old key");

        // Another certificate of the machine: not this hook's business.
        let other = dir.path().join("live/mail.example");
        std::fs::create_dir_all(&other).unwrap();
        assert!(run_hook(&hook, &other).status.success());
        assert_eq!(read("cert.pem"), "old cert");

        // A whole renewal.
        std::fs::write(lineage.join("privkey.pem"), "new key").unwrap();
        let done = run_hook(&hook, &lineage);
        assert!(
            done.status.success(),
            "{}",
            String::from_utf8_lossy(&done.stderr)
        );
        assert_eq!(
            (read("cert.pem"), read("key.pem")),
            ("new cert".into(), "new key".into())
        );
        let mode = |name: &str| {
            std::fs::metadata(tls.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!((mode("key.pem"), mode("cert.pem")), (0o600, 0o644));
        let left: Vec<_> = std::fs::read_dir(&tls)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left.len(), 2, "{left:?}");
    }

    #[test]
    fn paths_with_anything_in_them_are_quoted() {
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join("it's a $HOME; rm -rf x");
        std::fs::create_dir_all(odd.join("tls")).unwrap();
        let lineage = dir.path().join("live/nas.example");
        std::fs::create_dir_all(&lineage).unwrap();
        std::fs::write(lineage.join("fullchain.pem"), "c").unwrap();
        std::fs::write(lineage.join("privkey.pem"), "k").unwrap();
        let hook = walkthrough(&inputs(&odd)).unwrap().hook;
        let done = run_hook(&hook, &lineage);
        assert!(
            done.status.success(),
            "{}",
            String::from_utf8_lossy(&done.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(odd.join("tls/key.pem")).unwrap(),
            "k"
        );
    }

    #[test]
    fn the_docker_hook_goes_through_exec_and_is_valid_shell() {
        let dir = tempfile::tempdir().unwrap();
        let made = walkthrough(&Inputs {
            docker: true,
            ..inputs(dir.path())
        })
        .unwrap();
        for part in ["docker compose exec -T server", "COMPOSE_DIR=", "umask"] {
            assert!(made.hook.contains(part), "{}", made.hook);
        }
        assert!(!made.hook.contains("docker compose cp "));
        assert!(made.text.contains("Set COMPOSE_DIR"));
        let file = dir.path().join("hook");
        std::fs::write(&file, &made.hook).unwrap();
        let checked = Command::new("sh").arg("-n").arg(&file).output().unwrap();
        assert!(
            checked.status.success(),
            "{}",
            String::from_utf8_lossy(&checked.stderr)
        );
    }
}
