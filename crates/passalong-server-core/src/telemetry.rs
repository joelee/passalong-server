//! Logging: to standard error, at one of five levels, and only the fields on
//! an allow-list.
//!
//! | Level     | `tracing` | syslog    |
//! |-----------|-----------|-----------|
//! | `error`   | `ERROR`   | `err`     |
//! | `warning` | `WARN`    | `warning` |
//! | `info`    | `INFO`    | `info`    |
//! | `verbose` | `DEBUG`   | `notice`  |
//! | `debug`   | `TRACE`   | `debug`   |
//!
//! The mapping and the allow-list are the passalong client's. No call in
//! this crate logs an item's `meta`, content, a header, or any part of a
//! key's secret, and `tests/logs.rs` searches for them; the allow-list is
//! the second line of defence, for the call somebody adds later.

use std::fmt;

use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::{FormatFields, Writer};

pub use crate::config::LogLevel;

/// The only field names that are ever written. Ids, counts, outcomes, and
/// what a store said; never content, `meta`, a header, or a secret.
pub const ALLOWED_FIELDS: &[&str] = &[
    "message",
    "action",
    "cleanup",
    "err",
    "forced",
    "from",
    "generation",
    "generations_corrected",
    "holder",
    "item",
    "key",
    "path",
    "schema",
    "staging_places_removed",
    "to",
    "upload",
    "workspace",
];

impl LogLevel {
    /// The `tracing` level that shows what this level should.
    pub fn filter(self) -> LevelFilter {
        match self {
            Self::Error => LevelFilter::ERROR,
            Self::Warning => LevelFilter::WARN,
            Self::Info => LevelFilter::INFO,
            Self::Verbose => LevelFilter::DEBUG,
            Self::Debug => LevelFilter::TRACE,
        }
    }
}

/// Writes the fields that are on the list, and drops the rest.
struct Listed;

struct ListedVisitor<'a, 'w> {
    writer: &'a mut Writer<'w>,
    result: fmt::Result,
    first: bool,
}

impl Visit for ListedVisitor<'_, '_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if self.result.is_err() || !ALLOWED_FIELDS.contains(&field.name()) {
            return;
        }
        let gap = if self.first { "" } else { " " };
        self.first = false;
        self.result = if field.name() == "message" {
            write!(self.writer, "{gap}{value:?}")
        } else {
            write!(self.writer, "{gap}{}={value:?}", field.name())
        };
    }
}

impl<'w> FormatFields<'w> for Listed {
    fn format_fields<R: RecordFields>(&self, mut writer: Writer<'w>, fields: R) -> fmt::Result {
        let mut visitor = ListedVisitor {
            writer: &mut writer,
            result: Ok(()),
            first: true,
        };
        fields.record(&mut visitor);
        visitor.result
    }
}

/// A subscriber that writes to `writer`: timestamp, level, target, message,
/// and listed fields.
pub fn subscriber<W>(level: LogLevel, writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_max_level(level.filter())
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .fmt_fields(Listed)
        .finish()
}

/// Starts logging to standard error for the rest of the process. Standard
/// output stays the operator's: what a command prints is not a log.
pub fn init(level: LogLevel) {
    // A second call, as in tests that run commands in one process, changes
    // nothing, which is what is wanted.
    let _ = tracing::subscriber::set_global_default(subscriber(level, std::io::stderr));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn logged(level: LogLevel, emit: impl FnOnce()) -> String {
        let buffer = Buffer::default();
        let sink = buffer.clone();
        let subscriber = subscriber(level, move || sink.clone());
        tracing::subscriber::with_default(subscriber, emit);
        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn only_fields_on_the_list_are_written() {
        let text = logged(LogLevel::Info, || {
            tracing::info!(target: "passalong_server::test", key = "3f9a1c07b2e4", secret = "hunter2", meta = "SECRET-META", "key created");
        });
        assert!(
            text.contains("key created") && text.contains("3f9a1c07b2e4"),
            "{text}"
        );
        assert!(
            text.contains("INFO") && text.contains("passalong_server::test"),
            "{text}"
        );
        for leaked in ["hunter2", "secret", "SECRET-META", "meta"] {
            assert!(!text.contains(leaked), "`{leaked}` in: {text}");
        }
        // A timestamp leads the line.
        assert!(text.trim_start().starts_with("20"), "{text}");
    }

    #[test]
    fn each_level_shows_what_it_should() {
        let emit = || {
            tracing::error!("an error");
            tracing::warn!("a warning");
            tracing::info!("an info");
            tracing::debug!("a verbose");
            tracing::trace!("a debug");
        };
        let shown = |level| {
            let text = logged(level, emit);
            ["an error", "a warning", "an info", "a verbose", "a debug"]
                .map(|line| text.contains(line))
        };
        assert_eq!(shown(LogLevel::Error), [true, false, false, false, false]);
        assert_eq!(shown(LogLevel::Warning), [true, true, false, false, false]);
        assert_eq!(shown(LogLevel::Info), [true, true, true, false, false]);
        assert_eq!(shown(LogLevel::Verbose), [true, true, true, true, false]);
        assert_eq!(shown(LogLevel::Debug), [true, true, true, true, true]);
    }

    #[test]
    fn every_field_this_crate_logs_is_on_the_list() {
        // Otherwise a log line would silently lose what it was written for.
        let mut missing = Vec::new();
        for file in [
            "src/shelf/fs.rs",
            "src/ledger/sqlite.rs",
            "src/control/mod.rs",
            "src/upload.rs",
            "src/rewrite.rs",
            "src/workspace.rs",
        ] {
            let text = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file),
            )
            .unwrap();
            for line in text
                .lines()
                .filter(|line| line.contains("tracing::") && line.contains("target:"))
            {
                for part in line.split(',').skip(1) {
                    let part = part.trim();
                    let Some((name, _)) = part.split_once('=') else {
                        continue;
                    };
                    let name = name.trim().trim_start_matches(['%', '?']);
                    if name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                        && !ALLOWED_FIELDS.contains(&name)
                    {
                        missing.push(format!("{file}: {name}"));
                    }
                }
                // Shorthand fields: `%err`, `?cleanup`.
                for part in line
                    .split(',')
                    .map(str::trim)
                    .filter(|part| part.starts_with(['%', '?']))
                {
                    let name = part.trim_start_matches(['%', '?']);
                    if !name.contains('=') && !name.contains('.') && !ALLOWED_FIELDS.contains(&name)
                    {
                        missing.push(format!("{file}: {name}"));
                    }
                }
            }
        }
        assert!(missing.is_empty(), "logged but not allowed: {missing:?}");
    }
}
