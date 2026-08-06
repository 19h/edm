//! Filesystem-backed reconstruction of Elite Dangerous's local commander facts.
//!
//! The parser and state model live in `edm-core`; this module only applies a
//! deterministic, bounded ordering policy to journal files and live sidecars.

use std::cmp::Ordering;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::ports::Fs;
use edm_core::domain::commander::CommanderState;

const MAX_JOURNAL_FILES: usize = 256;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_JOURNAL_LINE_BYTES: usize = 1024 * 1024;

const FRONTIER_JOURNAL_DIR: [&str; 3] = ["Saved Games", "Frontier Developments", "Elite Dangerous"];
const PROTON_PREFIX: [&str; 6] = [
    "pfx",
    "drive_c",
    "users",
    "steamuser",
    "Saved Games",
    "Frontier Developments",
];
const STEAM_APP_ID: &str = "359320";

#[derive(Debug)]
struct JournalFile {
    path: PathBuf,
    contents: String,
    first_timestamp: Option<i128>,
}

/// Load all immediate journal files in `dir`, then merge Frontier's live
/// sidecars over the reconstructed state.
///
/// A later `LoadGame` is intentionally allowed to reset facts from an earlier
/// file. Journal files are therefore ordered by their first usable embedded
/// timestamp rather than by Frontier's filename. Files with no usable
/// timestamp sort first and use their full path as a deterministic fallback.
///
/// # Errors
///
/// Returns an error when directory or file I/O fails, or when an input bound
/// would be exceeded. A missing sidecar is not an error.
pub fn load_directory<F: Fs>(fs: &F, dir: &Path) -> Result<CommanderState, String> {
    let entries = fs
        .read_dir(dir)
        .map_err(|error| format!("failed to read commander directory: {error}"))?;

    let mut journal_paths = entries
        .into_iter()
        .filter(|path| path.parent() == Some(dir))
        .filter(|path| is_journal_path(path))
        .collect::<Vec<_>>();
    journal_paths.sort();
    journal_paths.dedup();

    if journal_paths.len() > MAX_JOURNAL_FILES {
        return Err(format!(
            "commander directory contains more than {MAX_JOURNAL_FILES} journal files"
        ));
    }

    let mut total_bytes = 0_usize;
    let mut journals = Vec::with_capacity(journal_paths.len());
    for path in journal_paths {
        let contents = fs
            .read_to_string(&path)
            .map_err(|error| format!("failed to read journal {}: {error}", path.display()))?;
        charge_total(&mut total_bytes, contents.len())?;
        validate_journal_lines(&contents)?;
        let first_timestamp = first_embedded_timestamp(&contents);
        journals.push(JournalFile {
            path,
            contents,
            first_timestamp,
        });
    }

    journals.sort_by(compare_journals);

    let mut state = CommanderState::default();
    for journal in journals {
        replay_journal(&mut state, &journal.contents);
    }

    merge_sidecar(
        fs,
        dir,
        "Status.json",
        &mut total_bytes,
        &mut state,
        CommanderState::merge_status_sidecar,
    )?;
    merge_sidecar(
        fs,
        dir,
        "Cargo.json",
        &mut total_bytes,
        &mut state,
        CommanderState::merge_cargo_sidecar,
    )?;
    merge_sidecar(
        fs,
        dir,
        "NavRoute.json",
        &mut total_bytes,
        &mut state,
        CommanderState::merge_nav_route_sidecar,
    )?;
    merge_sidecar(
        fs,
        dir,
        "Market.json",
        &mut total_bytes,
        &mut state,
        CommanderState::merge_market_sidecar,
    )?;

    Ok(state)
}

/// Build deterministic journal-directory candidates from ambient paths which
/// the caller has already obtained safely.
///
/// `steam_compat` is the value conventionally exposed as
/// `STEAM_COMPAT_DATA_PATH` (the app's `compatdata/359320` directory). An
/// explicitly requested journal directory is deliberately not included here;
/// callers should try it before these automatic candidates.
#[must_use]
pub fn auto_candidates(home: &Path, steam_compat: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(compat) = steam_compat {
        push_unique(&mut candidates, proton_journal_dir(compat));
    }

    // Native/common locations. The first is also useful when `home` is a
    // Windows profile supplied by a platform-neutral caller.
    push_unique(
        &mut candidates,
        join_components(home, &FRONTIER_JOURNAL_DIR),
    );
    push_unique(
        &mut candidates,
        home.join(".local")
            .join("share")
            .join("Frontier Developments")
            .join("Elite Dangerous"),
    );
    push_unique(
        &mut candidates,
        home.join("Library")
            .join("Application Support")
            .join("Frontier Developments")
            .join("Elite Dangerous"),
    );

    // Steam's common native, legacy, Debian-package, and Flatpak roots.
    for steam_root in [
        home.join(".local").join("share").join("Steam"),
        home.join(".steam").join("steam"),
        home.join(".steam").join("root"),
        home.join(".steam").join("debian-installation"),
        home.join(".var")
            .join("app")
            .join("com.valvesoftware.Steam")
            .join(".local")
            .join("share")
            .join("Steam"),
    ] {
        let compat = steam_root
            .join("steamapps")
            .join("compatdata")
            .join(STEAM_APP_ID);
        push_unique(&mut candidates, proton_journal_dir(&compat));
    }

    candidates
}

fn is_journal_path(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("log")
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| {
                stem.strip_prefix("Journal.")
                    .or_else(|| stem.strip_prefix("JournalBeta."))
                    .is_some_and(|suffix| !suffix.is_empty())
            })
}

fn compare_journals(left: &JournalFile, right: &JournalFile) -> Ordering {
    match (left.first_timestamp, right.first_timestamp) {
        (Some(left_timestamp), Some(right_timestamp)) => left_timestamp
            .cmp(&right_timestamp)
            .then_with(|| left.path.cmp(&right.path)),
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (None, None) => left.path.cmp(&right.path),
    }
}

fn charge_total(total: &mut usize, additional: usize) -> Result<(), String> {
    let Some(next) = total.checked_add(additional) else {
        return Err("commander files exceed the 64 MiB total input limit".to_owned());
    };
    if next > MAX_TOTAL_BYTES {
        return Err("commander files exceed the 64 MiB total input limit".to_owned());
    }
    *total = next;
    Ok(())
}

fn validate_journal_lines(contents: &str) -> Result<(), String> {
    let mut line_start = 0_usize;
    for (offset, byte) in contents.bytes().enumerate() {
        if byte != b'\n' {
            continue;
        }
        if offset - line_start > MAX_JOURNAL_LINE_BYTES {
            return Err("journal line exceeds the 1 MiB input limit".to_owned());
        }
        line_start = offset + 1;
    }
    if contents.len() - line_start > MAX_JOURNAL_LINE_BYTES {
        return Err("journal line exceeds the 1 MiB input limit".to_owned());
    }
    Ok(())
}

fn replay_journal(state: &mut CommanderState, contents: &str) {
    let mut lines = contents.split('\n').peekable();
    let mut line_number = 0_usize;
    while let Some(raw_line) = lines.next() {
        line_number += 1;
        // split() exposes the empty record after a terminating newline. It is
        // not a malformed journal event and is especially common in the file
        // which the game is currently appending to.
        if lines.peek().is_none() && raw_line.is_empty() {
            continue;
        }
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        state.apply_journal_json(line, line_number);
    }
}

fn merge_sidecar<F: Fs>(
    fs: &F,
    dir: &Path,
    name: &str,
    total_bytes: &mut usize,
    state: &mut CommanderState,
    merge: fn(&mut CommanderState, &str) -> bool,
) -> Result<(), String> {
    let path = dir.join(name);
    match fs.read_to_string(&path) {
        Ok(contents) => {
            charge_total(total_bytes, contents.len())?;
            merge(state, &contents);
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to read {name}: {error}")),
    }
}

fn first_embedded_timestamp(contents: &str) -> Option<i128> {
    contents.lines().find_map(timestamp_in_json_record)
}

/// Find a string-valued `timestamp` member without adding JSON parsing to the
/// impure crate. Full JSON validation remains the core parser's job during
/// replay. This lexer skips quoted contents, so text which merely mentions a
/// timestamp cannot affect file ordering.
fn timestamp_in_json_record(record: &str) -> Option<i128> {
    let bytes = record.as_bytes();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        if bytes[cursor] != b'"' {
            cursor += 1;
            continue;
        }
        let (key, after_key) = json_string(bytes, cursor)?;
        cursor = after_key;
        if key != b"timestamp" {
            continue;
        }

        let mut value_start = skip_ascii_whitespace(bytes, cursor);
        if bytes.get(value_start) != Some(&b':') {
            continue;
        }
        value_start = skip_ascii_whitespace(bytes, value_start + 1);
        if bytes.get(value_start) != Some(&b'"') {
            continue;
        }
        let (raw_timestamp, after_value) = json_string(bytes, value_start)?;
        cursor = after_value;
        if raw_timestamp.contains(&b'\\') {
            continue;
        }
        let Ok(timestamp) = std::str::from_utf8(raw_timestamp) else {
            continue;
        };
        if let Some(key) = timestamp_key(timestamp) {
            return Some(key);
        }
    }
    None
}

/// Return the raw bytes inside a JSON string and the first position after it.
fn json_string(bytes: &[u8], quote: usize) -> Option<(&[u8], usize)> {
    if bytes.get(quote) != Some(&b'"') {
        return None;
    }
    let mut cursor = quote + 1;
    while let Some(byte) = bytes.get(cursor) {
        match byte {
            b'"' => return Some((&bytes[quote + 1..cursor], cursor + 1)),
            b'\\' => {
                cursor += 2;
            }
            byte if *byte < b' ' => return None,
            _ => cursor += 1,
        }
    }
    None
}

fn skip_ascii_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes
        .get(cursor)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        cursor += 1;
    }
    cursor
}

/// Parse the RFC3339 subset Frontier writes and convert it to a sortable UTC
/// nanosecond key. Leap seconds and numeric offsets are accepted.
fn timestamp_key(raw: &str) -> Option<i128> {
    let bytes = raw.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }

    let year = i64::from(parse_digits(bytes, 0, 4)?);
    let month = parse_digits(bytes, 5, 2)?;
    let day = parse_digits(bytes, 8, 2)?;
    let hour = parse_digits(bytes, 11, 2)?;
    let minute = parse_digits(bytes, 14, 2)?;
    let second = parse_digits(bytes, 17, 2)?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut cursor = 19_usize;
    let mut nanos = 0_i128;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        let mut digits = 0_usize;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            if digits < 9 {
                nanos = nanos * 10 + i128::from(bytes[cursor] - b'0');
            }
            digits += 1;
            cursor += 1;
        }
        if cursor == start {
            return None;
        }
        for _ in digits.min(9)..9 {
            nanos *= 10;
        }
    }

    let offset_seconds = match bytes.get(cursor) {
        Some(b'Z' | b'z') if cursor + 1 == bytes.len() => 0_i64,
        Some(sign @ (b'+' | b'-')) if cursor + 6 == bytes.len() => {
            if bytes.get(cursor + 3) != Some(&b':') {
                return None;
            }
            let hours = i64::from(parse_digits(bytes, cursor + 1, 2)?);
            let minutes = i64::from(parse_digits(bytes, cursor + 4, 2)?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            let value = hours * 3600 + minutes * 60;
            if *sign == b'+' { value } else { -value }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let local_seconds =
        days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    Some(i128::from(local_seconds - offset_seconds) * 1_000_000_000 + nanos)
}

fn parse_digits(bytes: &[u8], start: usize, count: usize) -> Option<u32> {
    let mut value = 0_u32;
    for byte in bytes.get(start..start + count)? {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u32::from(*byte - b'0');
    }
    Some(value)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn join_components(base: &Path, components: &[&str]) -> PathBuf {
    components
        .iter()
        .fold(base.to_path_buf(), |path, component| path.join(component))
}

fn proton_journal_dir(compat: &Path) -> PathBuf {
    join_components(compat, &PROTON_PREFIX).join("Elite Dangerous")
}

fn push_unique(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !candidates.contains(&candidate) {
        candidates.push(candidate);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;

    use super::*;
    use crate::ports::RecordingFs;
    use edm_core::domain::commander::{ObservationSource, WarningCode};

    fn put(fs: &RecordingFs, path: &str, contents: &str) {
        fs.write(Path::new(path), contents)
            .expect("recording write");
    }

    #[test]
    fn timestamp_order_handles_continuations_and_a_later_session() {
        let fs = RecordingFs::default();
        put(
            &fs,
            "/journal/Journal.z.log",
            concat!(
                r#"{"timestamp":"2025-01-01T00:00:00Z","event":"LoadGame","Credits":10}"#,
                "\n",
                r#"{"timestamp":"2025-01-01T00:00:01Z","event":"Location","StarSystem":"First"}"#,
                "\n",
            ),
        );
        // Its filename sorts first, but its timestamp makes it a continuation
        // of the later LoadGame below.
        put(
            &fs,
            "/journal/Journal.a.log",
            concat!(
                r#"{"timestamp":"2025-02-01T00:00:02Z","event":"FSDJump","StarSystem":"Continued"}"#,
                "\n",
            ),
        );
        put(
            &fs,
            "/journal/JournalBeta.new.log",
            concat!(
                r#"{"timestamp":"2025-02-01T00:00:00Z","event":"LoadGame","Credits":20}"#,
                "\n",
                r#"{"timestamp":"2025-02-01T00:00:01Z","event":"Location","StarSystem":"Latest"}"#,
                "\n",
            ),
        );

        let state = load_directory(&fs, Path::new("/journal")).expect("load");
        assert_eq!(state.credits.as_ref().map(|value| value.value), Some(20));
        assert_eq!(
            state
                .current_system
                .as_ref()
                .map(|value| value.value.name.as_str()),
            Some("Continued")
        );
    }

    #[test]
    fn integer_ids_above_two_to_the_fifty_third_are_exact_and_identity_is_absent() {
        let fs = RecordingFs::default();
        put(
            &fs,
            "/journal/Journal.ids.log",
            concat!(
                r#"{"timestamp":"2025-01-01T00:00:00Z","event":"LoadGame","Commander":"Secret Name","FID":"F123","Credits":9007199254740993}"#,
                "\n",
                r#"{"timestamp":"2025-01-01T00:00:01Z","event":"Location","StarSystem":"Exact","SystemAddress":9007199254740995}"#,
            ),
        );

        let state = load_directory(&fs, Path::new("/journal")).expect("load");
        assert_eq!(
            state.credits.as_ref().map(|credits| credits.value),
            Some(9_007_199_254_740_993)
        );
        assert_eq!(
            state
                .current_system
                .as_ref()
                .and_then(|system| system.value.address),
            Some(9_007_199_254_740_995)
        );
        assert!(!format!("{state:?}").contains("Secret Name"));
        assert!(!format!("{state:?}").contains("F123"));
    }

    #[test]
    fn sidecars_merge_over_default_state_and_malformed_sidecars_warn() {
        let fs = RecordingFs::default();
        put(
            &fs,
            "/journal/Status.json",
            r#"{"timestamp":"2025-01-01T00:00:00Z","Flags":1,"Cargo":3}"#,
        );
        put(
            &fs,
            "/journal/Cargo.json",
            r#"{"timestamp":"2025-01-01T00:00:01Z","Vessel":"Ship","Count":3,"Inventory":[{"Name":"gold","Count":3,"Stolen":0}]}"#,
        );
        put(
            &fs,
            "/journal/NavRoute.json",
            r#"{"timestamp":"2025-01-01T00:00:02Z","Route":[{"StarSystem":"Goal","SystemAddress":9007199254740997}]}"#,
        );
        put(&fs, "/journal/Market.json", "{");

        let state = load_directory(&fs, Path::new("/journal")).expect("load");
        assert!(state.is_docked());
        assert_eq!(state.cargo.used_value(), Some(3));
        assert_eq!(
            state.nav_route.as_ref().map(|route| route.value.hops.len()),
            Some(1)
        );
        assert!(state.warnings.iter().any(|warning| {
            warning.code == WarningCode::MalformedJson
                && warning.source == Some(ObservationSource::MarketSidecar)
        }));
    }

    #[test]
    fn malformed_journal_line_warns_but_empty_final_line_does_not() {
        let fs = RecordingFs::default();
        put(
            &fs,
            "/journal/Journal.bad.log",
            concat!(
                r#"{"timestamp":"2025-01-01T00:00:00Z","event":"LoadGame","Credits":1}"#,
                "\nnot-json\n",
            ),
        );

        let state = load_directory(&fs, Path::new("/journal")).expect("load");
        assert_eq!(state.warnings.len(), 1);
        assert_eq!(state.warnings[0].code, WarningCode::MalformedJson);
        assert_eq!(state.warnings[0].line, Some(2));
    }

    #[test]
    fn file_line_and_total_bounds_are_enforced() {
        let fs = RecordingFs::default();
        for index in 0..=MAX_JOURNAL_FILES {
            put(&fs, &format!("/journal/Journal.{index:03}.log"), "");
        }
        let error = load_directory(&fs, Path::new("/journal")).expect_err("file bound");
        assert!(error.contains("more than 256"));

        let fs = RecordingFs::default();
        put(
            &fs,
            "/journal/Journal.long.log",
            &"x".repeat(MAX_JOURNAL_LINE_BYTES + 1),
        );
        let error = load_directory(&fs, Path::new("/journal")).expect_err("line bound");
        assert!(error.contains("1 MiB"));

        let mut total = MAX_TOTAL_BYTES;
        let error = charge_total(&mut total, 1).expect_err("total bound");
        assert!(error.contains("64 MiB"));
    }

    #[derive(Debug)]
    struct DirectoryFs {
        entries: io::Result<Vec<PathBuf>>,
        reads: RefCell<Vec<(PathBuf, io::Result<String>)>>,
    }

    impl Fs for DirectoryFs {
        fn write(&self, _path: &Path, _contents: &str) -> io::Result<()> {
            unreachable!("not used")
        }

        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            let mut reads = self.reads.borrow_mut();
            let index = reads
                .iter()
                .position(|(candidate, _)| candidate == path)
                .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "not found"))?;
            reads.remove(index).1
        }

        fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
            unreachable!("not used")
        }

        fn read_dir(&self, _path: &Path) -> io::Result<Vec<PathBuf>> {
            self.entries
                .as_ref()
                .map(Clone::clone)
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        }

        fn exists(&self, _path: &Path) -> bool {
            false
        }
    }

    #[test]
    fn no_journals_is_default_but_unreadable_input_is_an_error() {
        let empty = DirectoryFs {
            entries: Ok(Vec::new()),
            reads: RefCell::new(Vec::new()),
        };
        let state = load_directory(&empty, Path::new("/journal")).expect("empty directory");
        assert_eq!(state, CommanderState::default());

        let unreadable_directory = DirectoryFs {
            entries: Err(io::Error::new(ErrorKind::PermissionDenied, "denied")),
            reads: RefCell::new(Vec::new()),
        };
        let error = load_directory(&unreadable_directory, Path::new("/journal"))
            .expect_err("directory error");
        assert!(error.contains("failed to read commander directory"));

        let unreadable_journal = DirectoryFs {
            entries: Ok(vec![PathBuf::from("/journal/Journal.one.log")]),
            reads: RefCell::new(vec![(
                PathBuf::from("/journal/Journal.one.log"),
                Err(io::Error::new(ErrorKind::PermissionDenied, "denied")),
            )]),
        };
        let error =
            load_directory(&unreadable_journal, Path::new("/journal")).expect_err("journal error");
        assert!(error.contains("failed to read journal"));
    }

    #[test]
    fn sidecar_not_found_is_ignored_but_other_io_errors_are_not() {
        let fs = DirectoryFs {
            entries: Ok(Vec::new()),
            reads: RefCell::new(vec![(
                PathBuf::from("/journal/Cargo.json"),
                Err(io::Error::new(ErrorKind::PermissionDenied, "denied")),
            )]),
        };
        let error = load_directory(&fs, Path::new("/journal")).expect_err("sidecar error");
        assert!(error.contains("Cargo.json"));
    }

    #[test]
    fn candidate_paths_are_deterministic_and_platform_neutral() {
        let home = Path::new("/home/pilot");
        let compat = Path::new("/mnt/steam/compatdata/359320");
        let first = auto_candidates(home, Some(compat));
        let second = auto_candidates(home, Some(compat));
        assert_eq!(first, second);
        assert_eq!(first[0], proton_journal_dir(compat));
        assert!(
            first.contains(
                &home
                    .join("Library")
                    .join("Application Support")
                    .join("Frontier Developments")
                    .join("Elite Dangerous")
            )
        );
        assert!(first.iter().any(|path| {
            path == &home
                .join(".local")
                .join("share")
                .join("Steam")
                .join("steamapps")
                .join("compatdata")
                .join(STEAM_APP_ID)
                .join("pfx")
                .join("drive_c")
                .join("users")
                .join("steamuser")
                .join("Saved Games")
                .join("Frontier Developments")
                .join("Elite Dangerous")
        }));
        assert_eq!(
            first.len(),
            first.iter().collect::<std::collections::HashSet<_>>().len()
        );
    }

    #[test]
    fn timestamp_extraction_ignores_mentions_inside_strings_and_compares_offsets() {
        let line = r#"{"message":"\\\"timestamp\\\":\\\"1900-01-01T00:00:00Z\\\"","timestamp":"2025-01-01T01:00:00+01:00"}"#;
        assert_eq!(
            timestamp_in_json_record(line),
            timestamp_key("2025-01-01T00:00:00Z")
        );
    }
}
