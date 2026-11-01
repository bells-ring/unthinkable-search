//! Machine-readable result output, shared by `usc search`, `usc serve` and
//! documented in docs/INTEGRATION.md. No serde on the hot path: rows are
//! written straight into a reusable `String`.

use us_core::sort::{sort_ids, SortKey};
use us_core::{search, Index, Query};

/// Parse + run `q`, then order the result. Returns (ids, milliseconds).
pub fn query_sorted(ix: &Index, q: &str, key: SortKey, desc: bool) -> (Vec<u32>, f64) {
    let t = std::time::Instant::now();
    let mut ids = search::search(ix, &Query::parse(q), &std::sync::atomic::AtomicBool::new(false));
    if key != SortKey::Name || desc {
        sort_ids(ix, &mut ids, key, desc);
    }
    (ids, t.elapsed().as_secs_f64() * 1e3)
}

/// One output column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// Full path, e.g. `C:\Windows\notepad.exe` (a volume root is `C:\`).
    Full,
    /// File/folder name.
    Name,
    /// Containing folder, e.g. `C:\Windows` (empty for a volume root).
    Path,
    /// Bytes; folders: recursive size of all files below.
    Size,
    /// Last write time.
    Mtime,
    /// Creation time.
    Ctime,
    /// Windows file attributes (`FILE_ATTRIBUTE_*` bit mask).
    Attrs,
    /// `true` for folders.
    Dir,
}

pub const DEFAULT_FIELDS: &[Field] = &[Field::Full, Field::Name, Field::Path, Field::Size, Field::Mtime, Field::Ctime, Field::Attrs, Field::Dir];

impl Field {
    pub fn parse(s: &str) -> Option<Field> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "full" | "full_path" | "fullpath" => Field::Full,
            "name" => Field::Name,
            "path" | "dir_path" | "folder" => Field::Path,
            "size" => Field::Size,
            "mtime" | "modified" | "date_modified" => Field::Mtime,
            "ctime" | "created" | "date_created" => Field::Ctime,
            "attrs" | "attributes" => Field::Attrs,
            "dir" | "is_dir" | "isdir" => Field::Dir,
            _ => return None,
        })
    }

    pub fn key(self) -> &'static str {
        match self {
            Field::Full => "full",
            Field::Name => "name",
            Field::Path => "path",
            Field::Size => "size",
            Field::Mtime => "mtime",
            Field::Ctime => "ctime",
            Field::Attrs => "attrs",
            Field::Dir => "dir",
        }
    }
}

/// Comma-separated field list (`"full,size,mtime"`).
pub fn parse_fields(s: &str) -> Result<Vec<Field>, String> {
    s.split(',').filter(|x| !x.trim().is_empty()).map(|x| Field::parse(x).ok_or_else(|| format!("unknown field '{}'", x.trim()))).collect()
}

/// How timestamps are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TimeFormat {
    /// `2024-09-06T04:02:11Z` (UTC, seconds).
    #[default]
    Iso,
    /// Raw FILETIME: 100 ns ticks since 1601-01-01 UTC (exact).
    FileTime,
    /// Milliseconds since 1970-01-01 UTC.
    UnixMs,
}

impl TimeFormat {
    pub fn parse(s: &str) -> Option<TimeFormat> {
        Some(match s.to_ascii_lowercase().as_str() {
            "iso" | "iso8601" => TimeFormat::Iso,
            "filetime" | "ft" => TimeFormat::FileTime,
            "unix_ms" | "unixms" | "ms" => TimeFormat::UnixMs,
            _ => return None,
        })
    }
}

const FT_UNIX_EPOCH: i64 = 116_444_736_000_000_000; // 1970-01-01 in FILETIME ticks

pub fn filetime_to_unix_ms(ft: u64) -> i64 {
    (ft as i64 - FT_UNIX_EPOCH).div_euclid(10_000)
}

/// FILETIME → `YYYY-MM-DDTHH:MM:SSZ` (UTC). Pure arithmetic, no OS calls.
pub fn filetime_to_iso(ft: u64) -> String {
    let secs = (ft as i64 - FT_UNIX_EPOCH).div_euclid(10_000_000);
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil_from_days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", sod / 3600, sod / 60 % 60, sod % 60)
}

/// Append `s` as a JSON string literal.
pub fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn time_json(out: &mut String, ft: u64, tf: TimeFormat) {
    use std::fmt::Write;
    if ft == 0 {
        out.push_str("null");
        return;
    }
    match tf {
        TimeFormat::Iso => json_str(out, &filetime_to_iso(ft)),
        TimeFormat::FileTime => {
            let _ = write!(out, "{ft}");
        }
        TimeFormat::UnixMs => {
            let _ = write!(out, "{}", filetime_to_unix_ms(ft));
        }
    }
}

fn time_text(ft: u64, tf: TimeFormat) -> String {
    match (ft, tf) {
        (0, _) => String::new(),
        (_, TimeFormat::Iso) => filetime_to_iso(ft),
        (_, TimeFormat::FileTime) => ft.to_string(),
        (_, TimeFormat::UnixMs) => filetime_to_unix_ms(ft).to_string(),
    }
}

/// Append one result as a JSON object.
pub fn json_item(out: &mut String, ix: &Index, id: u32, fields: &[Field], tf: TimeFormat, scratch: &mut String) {
    use std::fmt::Write;
    out.push('{');
    for (k, f) in fields.iter().enumerate() {
        if k > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(f.key());
        out.push_str("\":");
        match f {
            Field::Full => {
                ix.full_path_into(id, scratch);
                json_str(out, scratch);
            }
            Field::Name => json_str(out, ix.name(id)),
            Field::Path => {
                ix.dir_path_into(id, scratch);
                json_str(out, scratch);
            }
            Field::Size => {
                let _ = write!(out, "{}", ix.total_size(id));
            }
            Field::Mtime => time_json(out, ix.mtime[id as usize], tf),
            Field::Ctime => time_json(out, ix.ctime[id as usize], tf),
            Field::Attrs => {
                let _ = write!(out, "{}", ix.attrs[id as usize]);
            }
            Field::Dir => out.push_str(if ix.is_dir(id) { "true" } else { "false" }),
        }
    }
    out.push('}');
}

/// Text value of one cell (TSV/CSV).
pub fn cell(ix: &Index, id: u32, f: Field, tf: TimeFormat, scratch: &mut String) -> String {
    match f {
        Field::Full => {
            ix.full_path_into(id, scratch);
            scratch.clone()
        }
        Field::Name => ix.name(id).to_string(),
        Field::Path => {
            ix.dir_path_into(id, scratch);
            scratch.clone()
        }
        Field::Size => ix.total_size(id).to_string(),
        Field::Mtime => time_text(ix.mtime[id as usize], tf),
        Field::Ctime => time_text(ix.ctime[id as usize], tf),
        Field::Attrs => ix.attrs[id as usize].to_string(),
        Field::Dir => (if ix.is_dir(id) { "1" } else { "0" }).to_string(),
    }
}

/// CSV (RFC 4180) quoting when needed.
pub fn csv_quote(out: &mut String, s: &str) {
    if s.contains([',', '"', '\n', '\r']) {
        out.push('"');
        out.push_str(&s.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(s);
    }
}

/// TSV: tabs/newlines cannot occur in Windows names; replaced defensively.
pub fn tsv_clean(out: &mut String, s: &str) {
    out.extend(s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(filetime_to_iso(FT_UNIX_EPOCH as u64), "1970-01-01T00:00:00Z");
        // 2024-09-06T04:02:11Z = 1725595331 s
        let ft = (1_725_595_331i64 * 10_000_000 + FT_UNIX_EPOCH) as u64;
        assert_eq!(filetime_to_iso(ft), "2024-09-06T04:02:11Z");
        assert_eq!(filetime_to_unix_ms(ft), 1_725_595_331_000);
        assert_eq!(filetime_to_iso(0), "1601-01-01T00:00:00Z");
        // leap day
        let ft = (951_782_400i64 * 10_000_000 + FT_UNIX_EPOCH) as u64; // 2000-02-29
        assert_eq!(filetime_to_iso(ft), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn escaping_and_fields() {
        let mut s = String::new();
        json_str(&mut s, "a\"b\\c\u{1}д");
        assert_eq!(s, r#""a\"b\\c\u0001д""#);
        assert_eq!(parse_fields("full, size,MTIME").unwrap(), vec![Field::Full, Field::Size, Field::Mtime]);
        assert!(parse_fields("full,bogus").is_err());
        let mut c = String::new();
        csv_quote(&mut c, "a,\"b\"");
        assert_eq!(c, r#""a,""b""""#);
    }
}
