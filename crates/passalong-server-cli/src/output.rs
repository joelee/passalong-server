//! What commands print: tables for people, JSON for scripts, and times an
//! operator can read.

/// `1767225600` as `2026-01-01T00:00:00Z`.
pub fn timestamp(unix: u64) -> String {
    let (days, rest) = (unix / 86_400, unix % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// A time, or `never`, or `-` for what has not happened.
pub fn when(unix: Option<u64>, absent: &str) -> String {
    unix.map_or_else(|| absent.to_owned(), timestamp)
}

/// `21474836480` as `20.0 GiB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Rows under headings, each column as wide as its widest cell.
pub fn table(headings: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headings.iter().map(|heading| heading.len()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        padded.join("  ").trim_end().to_owned()
    };
    let mut out = vec![line(headings.to_vec())];
    out.extend(
        rows.iter()
            .map(|row| line(row.iter().map(String::as_str).collect())),
    );
    out.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_utc_and_readable() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(1_767_225_600), "2026-01-01T00:00:00Z");
        assert_eq!(timestamp(1_709_251_199), "2024-02-29T23:59:59Z");
        assert_eq!(timestamp(4_102_444_800), "2100-01-01T00:00:00Z");
        assert_eq!(when(None, "never"), "never");
    }

    #[test]
    fn sizes_are_in_binary_units() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(20 << 30), "20.0 GiB");
    }

    #[test]
    fn a_table_lines_its_columns_up() {
        let text = table(
            &["ID", "LABEL"],
            &[
                vec!["3f9a".into(), "laptop".into()],
                vec!["b2".into(), "nas".into()],
            ],
        );
        assert_eq!(text, "ID    LABEL\n3f9a  laptop\nb2    nas\n");
    }
}
