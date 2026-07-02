use crate::wire::{Emitter, EntryKind};
use memchr::memchr_iter;
use std::ffi::OsString;
use std::fs::{self, DirEntry, File, FileTimes, FileType, OpenOptions};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn ls(args: &[String], emitter: &mut Emitter) {
    let mut show_all = false;
    let mut recursive = false;
    let mut paths = Vec::new();
    let mut options_ended = false;

    for arg in args {
        if !options_ended && arg == "--" {
            options_ended = true;
        } else if !options_ended && (arg == "-a" || arg == "--all") {
            show_all = true;
        } else if !options_ended && (arg == "-R" || arg == "--recursive") {
            recursive = true;
        } else if !options_ended && arg.starts_with('-') {
            emitter.error(
                "InvalidOption",
                "InvalidArgument",
                format!("ls の不明なオプションです: {arg}"),
                None,
            );
        } else {
            paths.push(PathBuf::from(arg));
        }
    }

    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }

    for path in paths {
        if emitter.is_stopped() {
            return;
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                if recursive {
                    walk_and_emit(&path, show_all, emitter);
                } else {
                    emit_directory_children(&path, show_all, emitter);
                }
            }
            Ok(metadata) => {
                emit_metadata(&path, &metadata, emitter);
            }
            Err(error) => emit_io_error("ListFailed", &path, error, emitter),
        }
    }
}

pub(crate) fn touch(args: &[String], emitter: &mut Emitter) {
    let paths: Vec<&str> = args
        .iter()
        .filter(|arg| arg.as_str() != "--")
        .map(String::as_str)
        .collect();
    if paths.is_empty() {
        emitter.error(
            "MissingPath",
            "InvalidArgument",
            "touch には少なくとも一つのパスが必要です",
            None,
        );
        return;
    }

    let modified = SystemTime::now();
    const PARALLEL_THRESHOLD: usize = 32;
    if paths.len() < PARALLEL_THRESHOLD {
        for path in paths {
            if !emit_touch_result(path, touch_path(Path::new(path), modified), emitter) {
                return;
            }
        }
    } else {
        touch_paths_parallel(&paths, modified, emitter);
    }
}

type TouchResult = Result<fs::Metadata, (&'static str, io::Error)>;

fn touch_paths_parallel(paths: &[&str], modified: SystemTime, emitter: &mut Emitter) {
    let worker_count = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(16)
        .min(paths.len());

    std::thread::scope(|scope| {
        let (job_sender, job_receiver) = mpsc::channel::<(usize, &str)>();
        let job_receiver = Arc::new(Mutex::new(job_receiver));
        let (result_sender, result_receiver) = mpsc::channel::<(usize, TouchResult)>();

        for _ in 0..worker_count {
            let job_receiver = Arc::clone(&job_receiver);
            let result_sender = result_sender.clone();
            scope.spawn(move || {
                loop {
                    let job = {
                        let receiver = job_receiver.lock().expect("touch job queue poisoned");
                        receiver.recv()
                    };
                    let Ok((index, path)) = job else {
                        break;
                    };
                    if result_sender
                        .send((index, touch_path(Path::new(path), modified)))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(result_sender);

        let mut scheduled = 0;
        while scheduled < worker_count {
            job_sender
                .send((scheduled, paths[scheduled]))
                .expect("touch workers stopped unexpectedly");
            scheduled += 1;
        }

        let mut pending: Vec<Option<TouchResult>> =
            std::iter::repeat_with(|| None).take(paths.len()).collect();
        let mut next_to_emit = 0;
        while next_to_emit < paths.len() {
            let (index, result) = result_receiver
                .recv()
                .expect("touch workers stopped unexpectedly");
            pending[index] = Some(result);

            while next_to_emit < paths.len() {
                let Some(result) = pending[next_to_emit].take() else {
                    break;
                };
                if !emit_touch_result(paths[next_to_emit], result, emitter) {
                    return;
                }
                next_to_emit += 1;

                if scheduled < paths.len() {
                    job_sender
                        .send((scheduled, paths[scheduled]))
                        .expect("touch workers stopped unexpectedly");
                    scheduled += 1;
                }
            }
        }
    });
}

fn emit_touch_result(path: &str, result: TouchResult, emitter: &mut Emitter) -> bool {
    let path = Path::new(path);
    match result {
        Ok(metadata) => emit_metadata(path, &metadata, emitter),
        Err((code, error)) => {
            emit_io_error(code, path, error, emitter);
            !emitter.is_stopped()
        }
    }
}

fn touch_path(path: &Path, modified: SystemTime) -> TouchResult {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| ("TouchFailed", error))?;
    file.set_times(FileTimes::new().set_modified(modified))
        .map_err(|error| ("TouchFailed", error))?;
    drop(file);
    fs::symlink_metadata(path).map_err(|error| ("MetadataFailed", error))
}

pub(crate) fn find(args: &[String], emitter: &mut Emitter) {
    let mut roots = Vec::new();
    let mut name_pattern = "*".to_owned();
    let mut kind_filter: Option<char> = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "-name" | "--name" => {
                index += 1;
                let Some(pattern) = args.get(index) else {
                    emitter.error(
                        "MissingPattern",
                        "InvalidArgument",
                        "find -name にはパターンが必要です",
                        None,
                    );
                    return;
                };
                name_pattern.clone_from(pattern);
            }
            "-type" | "--type" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    emitter.error(
                        "MissingType",
                        "InvalidArgument",
                        "find -type には f、d、l のいずれかが必要です",
                        None,
                    );
                    return;
                };
                kind_filter = match value.as_str() {
                    "f" => Some('f'),
                    "d" => Some('d'),
                    "l" => Some('l'),
                    _ => {
                        emitter.error(
                            "InvalidType",
                            "InvalidArgument",
                            format!("find -type の値が不正です: {value}"),
                            None,
                        );
                        return;
                    }
                };
            }
            "--" => {}
            value if value.starts_with('-') => {
                emitter.error(
                    "InvalidOption",
                    "InvalidArgument",
                    format!("find の不明なオプションです: {value}"),
                    None,
                );
            }
            value => roots.push(PathBuf::from(value)),
        }
        index += 1;
    }

    if roots.is_empty() {
        roots.push(PathBuf::from("."));
    }

    let name_pattern = WildcardPattern::new(&name_pattern);

    for root in roots {
        if emitter.is_stopped() {
            return;
        }
        find_root(&root, &name_pattern, kind_filter, emitter);
    }
}

pub(crate) fn cat(args: &[String], emitter: &mut Emitter) {
    if !args.iter().any(|arg| arg != "--") {
        emitter.error(
            "MissingPath",
            "InvalidArgument",
            "cat には少なくとも一つのパスが必要です",
            None,
        );
        return;
    }

    for arg in args.iter().filter(|arg| arg.as_str() != "--") {
        if emitter.is_stopped() {
            return;
        }
        let path = Path::new(arg);
        if path.as_os_str() == "-" {
            emitter.error(
                "UnsupportedStandardInput",
                "InvalidArgument",
                "標準入力には '-' ではなく PowerShell パイプラインを使用してください",
                Some(path.to_string_lossy().into_owned()),
            );
            continue;
        }
        match open_sequential(path) {
            Ok(file) => emit_file_lines(path, file, emitter),
            Err(error) => emit_io_error("ReadFailed", path, error, emitter),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SedLineRange {
    start: u64,
    end: Option<u64>,
}

pub(crate) fn sed(args: &[String], emitter: &mut Emitter) {
    let mut script = None;
    let mut paths = Vec::new();
    let mut options_ended = false;

    for arg in args {
        if !options_ended && arg == "--" {
            options_ended = true;
        } else if !options_ended && arg == "-n" {
            // FastFs sed never performs implicit printing, so -n is accepted as a no-op.
        } else if !options_ended && arg.starts_with('-') {
            emitter.error(
                "InvalidOption",
                "InvalidArgument",
                format!("sed の不明なオプションです: {arg}"),
                None,
            );
            return;
        } else if script.is_none() {
            script = Some(arg.as_str());
        } else {
            paths.push(arg.as_str());
        }
    }

    let Some(script) = script else {
        emitter.error(
            "MissingScript",
            "InvalidArgument",
            "sed には '10,20p' のような表示範囲が必要です",
            None,
        );
        return;
    };
    let range = match parse_sed_print_script(script) {
        Ok(range) => range,
        Err(message) => {
            emitter.error(
                "InvalidScript",
                "InvalidArgument",
                message,
                Some(script.to_owned()),
            );
            return;
        }
    };
    if paths.is_empty() {
        emitter.error(
            "MissingPath",
            "InvalidArgument",
            "sed には少なくとも一つのパスが必要です",
            None,
        );
        return;
    }

    for path in paths {
        if emitter.is_stopped() {
            return;
        }
        let path = Path::new(path);
        if path.as_os_str() == "-" {
            emitter.error(
                "UnsupportedStandardInput",
                "InvalidArgument",
                "標準入力には '-' ではなく PowerShell パイプラインを使用してください",
                Some(path.to_string_lossy().into_owned()),
            );
            continue;
        }
        match open_sequential(path) {
            Ok(file) => emit_file_range(path, file, range, emitter),
            Err(error) => emit_io_error("ReadFailed", path, error, emitter),
        }
    }
}

fn parse_sed_print_script(script: &str) -> Result<SedLineRange, String> {
    let script = script.trim();
    let Some(addresses) = script.strip_suffix('p') else {
        return Err("sed が対応するスクリプトは '10p'、'10,20p'、'10,$p' です".to_owned());
    };
    let mut parts = addresses.split(',');
    let start_text = parts.next().unwrap_or_default().trim();
    let end_text = parts.next().map(str::trim);
    if parts.next().is_some() || start_text.is_empty() {
        return Err("sed の表示範囲が不正です".to_owned());
    }

    let start = parse_positive_line_number(start_text)
        .ok_or_else(|| "sed の行番号は1以上の整数で指定してください".to_owned())?;
    let end = match end_text {
        None => Some(start),
        Some("") => return Err("sed の終了行が指定されていません".to_owned()),
        Some("$") => None,
        Some(value) => Some(
            parse_positive_line_number(value)
                .ok_or_else(|| "sed の行番号は1以上の整数で指定してください".to_owned())?,
        ),
    };
    if let Some(end) = end
        && end < start
    {
        return Err("sed の終了行は開始行以上でなければなりません".to_owned());
    }
    Ok(SedLineRange { start, end })
}

fn parse_positive_line_number(value: &str) -> Option<u64> {
    value.parse().ok().filter(|number| *number > 0)
}

fn emit_file_range(path: &Path, file: File, range: SedLineRange, emitter: &mut Emitter) {
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    match skip_lines(&mut reader, range.start - 1) {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            emit_io_error("ReadFailed", path, error, emitter);
            return;
        }
    }

    let mut remaining = range.end.map(|end| end - range.start + 1);
    let mut bytes = Vec::with_capacity(1024);
    loop {
        if remaining == Some(0) {
            return;
        }
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) => return,
            Ok(_) => {
                strip_line_ending(&mut bytes);
                let value = String::from_utf8_lossy(&bytes);
                if !emitter.text(&value) {
                    return;
                }
                if let Some(value) = &mut remaining {
                    *value -= 1;
                }
            }
            Err(error) => {
                emit_io_error("ReadFailed", path, error, emitter);
                return;
            }
        }
    }
}

fn skip_lines<R: BufRead>(reader: &mut R, mut count: u64) -> io::Result<bool> {
    while count > 0 {
        let (consumed, reached_target) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(false);
            }
            let mut consumed = available.len();
            let mut reached_target = false;
            for position in memchr_iter(b'\n', available) {
                count -= 1;
                if count == 0 {
                    consumed = position + 1;
                    reached_target = true;
                    break;
                }
            }
            (consumed, reached_target)
        };
        reader.consume(consumed);
        if reached_target {
            return Ok(true);
        }
    }
    Ok(true)
}

#[cfg(windows)]
fn open_sequential(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)
}

#[cfg(not(windows))]
fn open_sequential(path: &Path) -> io::Result<File> {
    File::open(path)
}

fn emit_file_lines(path: &Path, file: File, emitter: &mut Emitter) {
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut bytes = Vec::with_capacity(1024);
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) => break,
            Ok(_) => {
                strip_line_ending(&mut bytes);
                let value = String::from_utf8_lossy(&bytes);
                if !emitter.text(&value) {
                    return;
                }
            }
            Err(error) => {
                emit_io_error("ReadFailed", path, error, emitter);
                return;
            }
        }
    }
}

fn strip_line_ending(bytes: &mut Vec<u8>) {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
}

struct NamedEntry {
    name: OsString,
    entry: DirEntry,
}

fn walk_and_emit(root: &Path, show_all: bool, emitter: &mut Emitter) {
    let mut stack = Vec::new();
    if !push_children(root, show_all, true, &mut stack, "WalkFailed", emitter) {
        return;
    }

    while let Some(child) = stack.pop() {
        if emitter.is_stopped() {
            return;
        }
        let path = child.entry.path();
        let file_type = match child.entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                emit_io_error("WalkFailed", &path, error, emitter);
                continue;
            }
        };
        let metadata = match entry_metadata(&child.entry) {
            Ok(metadata) => metadata,
            Err(error) => {
                emit_io_error("WalkFailed", &path, error, emitter);
                continue;
            }
        };
        if !emit_metadata(&path, &metadata, emitter) {
            return;
        }

        if file_type.is_dir() && !file_type.is_symlink() {
            push_children(&path, show_all, true, &mut stack, "WalkFailed", emitter);
        }
    }
}

fn emit_directory_children(path: &Path, show_all: bool, emitter: &mut Emitter) {
    match directory_entries(path, show_all, true) {
        Ok(children) => {
            for child in children {
                if emitter.is_stopped() {
                    return;
                }
                let child_path = child.entry.path();
                match child.entry.metadata() {
                    Ok(metadata) => {
                        if !emit_metadata(&child_path, &metadata, emitter) {
                            return;
                        }
                    }
                    Err(error) => emit_io_error("MetadataFailed", &child_path, error, emitter),
                }
            }
        }
        Err(error) => emit_io_error("ListFailed", path, error, emitter),
    }
}

fn find_root(
    root: &Path,
    pattern: &WildcardPattern,
    kind_filter: Option<char>,
    emitter: &mut Emitter,
) {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) => {
            emit_io_error("WalkFailed", root, error, emitter);
            return;
        }
    };
    let root_type = metadata.file_type();
    if root
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| pattern.is_match(name))
        && matches_metadata_kind(&metadata, kind_filter)
        && !emit_metadata(root, &metadata, emitter)
    {
        return;
    }

    if !root_type.is_dir() || root_type.is_symlink() {
        return;
    }

    struct FindFrame {
        path: PathBuf,
        entries: fs::ReadDir,
    }

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            emit_io_error("WalkFailed", root, error, emitter);
            return;
        }
    };
    let mut stack = vec![FindFrame {
        path: root.to_path_buf(),
        entries,
    }];
    while !stack.is_empty() {
        if emitter.is_stopped() {
            return;
        }
        let next = stack.last_mut().and_then(|frame| frame.entries.next());
        let Some(next) = next else {
            stack.pop();
            continue;
        };
        let entry = match next {
            Ok(entry) => entry,
            Err(error) => {
                let directory = &stack.last().expect("find stack is not empty").path;
                emit_io_error("WalkFailed", directory, error, emitter);
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                let path = entry.path();
                emit_io_error("WalkFailed", &path, error, emitter);
                continue;
            }
        };

        let matches = entry
            .file_name()
            .to_str()
            .is_some_and(|name| pattern.is_match(name))
            && matches_file_type(file_type, kind_filter);
        let descend = file_type.is_dir() && !file_type.is_symlink();
        if !matches && !descend {
            continue;
        }

        let path = entry.path();
        if matches {
            match entry_metadata(&entry) {
                Ok(metadata) => {
                    if !emit_metadata(&path, &metadata, emitter) {
                        return;
                    }
                }
                Err(error) => emit_io_error("MetadataFailed", &path, error, emitter),
            }
        }

        if descend {
            match fs::read_dir(&path) {
                Ok(entries) => stack.push(FindFrame { path, entries }),
                Err(error) => emit_io_error("WalkFailed", &path, error, emitter),
            }
        }
    }
}

fn push_children(
    path: &Path,
    show_all: bool,
    sort: bool,
    stack: &mut Vec<NamedEntry>,
    error_code: &str,
    emitter: &mut Emitter,
) -> bool {
    match directory_entries(path, show_all, sort) {
        Ok(children) => {
            stack.extend(children.into_iter().rev());
            true
        }
        Err(error) => {
            emit_io_error(error_code, path, error, emitter);
            false
        }
    }
}

fn directory_entries(path: &Path, show_all: bool, sort: bool) -> io::Result<Vec<NamedEntry>> {
    let mut children = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        if !show_all && is_hidden_name(&name) {
            continue;
        }
        children.push(NamedEntry { name, entry });
    }
    if sort {
        children.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    }
    Ok(children)
}

fn entry_metadata(entry: &DirEntry) -> io::Result<fs::Metadata> {
    entry.metadata()
}

fn matches_file_type(file_type: FileType, kind_filter: Option<char>) -> bool {
    match kind_filter {
        Some('f') => file_type.is_file(),
        Some('d') => file_type.is_dir(),
        Some('l') => file_type.is_symlink(),
        _ => true,
    }
}

fn matches_metadata_kind(metadata: &fs::Metadata, kind_filter: Option<char>) -> bool {
    matches_file_type(metadata.file_type(), kind_filter)
}

fn is_hidden_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| name.starts_with('.'))
}

fn emit_metadata(path: &Path, metadata: &fs::Metadata, emitter: &mut Emitter) -> bool {
    let kind = if metadata.file_type().is_symlink() {
        EntryKind::Symlink
    } else if metadata.is_dir() {
        EntryKind::Directory
    } else if metadata.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    let modified_unix_milliseconds = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());

    let path_text = path.to_string_lossy();
    emitter.entry(
        &path_text,
        kind,
        metadata.is_file().then_some(metadata.len()),
        modified_unix_milliseconds,
        metadata.permissions().readonly(),
    )
}

fn emit_io_error(code: &str, path: &Path, error: io::Error, emitter: &mut Emitter) {
    let category = match error.kind() {
        io::ErrorKind::NotFound => "ObjectNotFound",
        io::ErrorKind::PermissionDenied => "PermissionDenied",
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => "InvalidArgument",
        _ => "NotSpecified",
    };
    emitter.error(
        code,
        category,
        format!("{}: {error}", path.display()),
        Some(path.to_string_lossy().into_owned()),
    );
}

enum WildcardPattern {
    Any,
    Exact(String),
    Prefix(String),
    Suffix(String),
    General {
        ascii: Option<Vec<u8>>,
        characters: Vec<char>,
    },
}

impl WildcardPattern {
    fn new(pattern: &str) -> Self {
        if !pattern.is_empty() && pattern.bytes().all(|byte| byte == b'*') {
            return Self::Any;
        }
        if !pattern.contains(['*', '?']) {
            return Self::Exact(pattern.to_owned());
        }
        if let Some(prefix) = pattern.strip_suffix('*')
            && !prefix.contains(['*', '?'])
        {
            return Self::Prefix(prefix.to_owned());
        }
        if let Some(suffix) = pattern.strip_prefix('*')
            && !suffix.contains(['*', '?'])
        {
            return Self::Suffix(suffix.to_owned());
        }
        Self::General {
            ascii: pattern.is_ascii().then(|| pattern.as_bytes().to_vec()),
            characters: pattern.chars().collect(),
        }
    }

    fn is_match(&self, value: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(pattern) => value == pattern,
            Self::Prefix(prefix) => value.starts_with(prefix),
            Self::Suffix(suffix) => value.ends_with(suffix),
            Self::General { ascii, characters } => {
                if let Some(pattern) = ascii
                    && value.is_ascii()
                {
                    wildcard_matches_slice(pattern, value.as_bytes(), b'*', b'?')
                } else {
                    wildcard_matches_text(characters, value)
                }
            }
        }
    }
}

#[cfg(test)]
fn wildcard_matches(pattern: &str, value: &str) -> bool {
    WildcardPattern::new(pattern).is_match(value)
}

fn wildcard_matches_slice<T: Copy + Eq>(pattern: &[T], value: &[T], star: T, question: T) -> bool {
    let mut pattern_index = 0;
    let mut value_index = 0;
    let mut last_star = None;
    let mut retry_value_index = 0;

    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == question || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == star {
            last_star = Some(pattern_index);
            pattern_index += 1;
            retry_value_index = value_index;
        } else if let Some(star_index) = last_star {
            pattern_index = star_index + 1;
            retry_value_index += 1;
            value_index = retry_value_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == star {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn wildcard_matches_text(pattern: &[char], value: &str) -> bool {
    let mut pattern_index = 0;
    let mut value_index = 0;
    let mut last_star = None;
    let mut retry_value_index = 0;

    while value_index < value.len() {
        let value_character = value[value_index..]
            .chars()
            .next()
            .expect("value_index is on a character boundary");
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == value_character)
        {
            pattern_index += 1;
            value_index += value_character.len_utf8();
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            last_star = Some(pattern_index);
            pattern_index += 1;
            retry_value_index = value_index;
        } else if let Some(star_index) = last_star {
            pattern_index = star_index + 1;
            let retry_character = value[retry_value_index..]
                .chars()
                .next()
                .expect("retry index precedes value end");
            retry_value_index += retry_character.len_utf8();
            value_index = retry_value_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::{SedLineRange, parse_sed_print_script, skip_lines, wildcard_matches};
    use std::io::{BufReader, Cursor, Read};

    #[test]
    fn sed_print_script_supports_single_bounded_and_open_ranges() {
        assert_eq!(
            parse_sed_print_script("10p"),
            Ok(SedLineRange {
                start: 10,
                end: Some(10),
            })
        );
        assert_eq!(
            parse_sed_print_script(" 10,20p "),
            Ok(SedLineRange {
                start: 10,
                end: Some(20),
            })
        );
        assert_eq!(
            parse_sed_print_script("10,$p"),
            Ok(SedLineRange {
                start: 10,
                end: None,
            })
        );
    }

    #[test]
    fn sed_print_script_rejects_writes_and_invalid_ranges() {
        for script in ["s/a/b/", "10,5p", "0p", "10,20d", "10,,20p", "p"] {
            assert!(parse_sed_print_script(script).is_err(), "script={script:?}");
        }
    }

    #[test]
    fn sed_skip_lines_handles_buffer_and_eof_boundaries() {
        let mut reader = BufReader::with_capacity(3, Cursor::new(b"a\nbb\nccc"));
        assert!(skip_lines(&mut reader, 2).unwrap());
        let mut remainder = String::new();
        reader.read_to_string(&mut remainder).unwrap();
        assert_eq!(remainder, "ccc");

        let mut reader = BufReader::with_capacity(2, Cursor::new(b"a\nb"));
        assert!(!skip_lines(&mut reader, 2).unwrap());
    }

    #[test]
    fn wildcard_supports_star_and_question_mark() {
        assert!(wildcard_matches("*.txt", "notes.txt"));
        assert!(wildcard_matches("file?.txt", "file1.txt"));
        assert!(!wildcard_matches("*.txt", "notes.md"));
        assert!(!wildcard_matches("file?.txt", "file10.txt"));
    }

    #[test]
    fn wildcard_handles_unicode_characters() {
        assert!(wildcard_matches("資料?.txt", "資料１.txt"));
    }

    #[test]
    fn wildcard_handles_empty_and_repeated_stars() {
        assert!(wildcard_matches("", ""));
        assert!(wildcard_matches("**", "anything"));
        assert!(wildcard_matches("a**?c", "abbbc"));
        assert!(!wildcard_matches("?", ""));
    }

    #[test]
    fn greedy_wildcard_matches_reference_implementation() {
        let patterns = generate_strings(&['a', 'b', '*', '?'], 4);
        let values = generate_strings(&['a', 'b'], 4);
        for pattern in patterns {
            for value in &values {
                assert_eq!(
                    wildcard_matches(&pattern, value),
                    wildcard_reference(&pattern, value),
                    "pattern={pattern:?}, value={value:?}"
                );
            }
        }
    }

    fn generate_strings(alphabet: &[char], maximum_length: usize) -> Vec<String> {
        let mut values = vec![String::new()];
        let mut current = vec![String::new()];
        for _ in 0..maximum_length {
            let mut next = Vec::new();
            for prefix in &current {
                for character in alphabet {
                    let mut value = prefix.clone();
                    value.push(*character);
                    next.push(value);
                }
            }
            values.extend(next.iter().cloned());
            current = next;
        }
        values
    }

    fn wildcard_reference(pattern: &str, value: &str) -> bool {
        let pattern: Vec<char> = pattern.chars().collect();
        let value: Vec<char> = value.chars().collect();
        let mut previous = vec![false; value.len() + 1];
        previous[0] = true;
        for token in pattern {
            let mut current = vec![false; value.len() + 1];
            if token == '*' {
                current[0] = previous[0];
            }
            for index in 1..=value.len() {
                current[index] = match token {
                    '*' => previous[index] || current[index - 1],
                    '?' => previous[index - 1],
                    literal => previous[index - 1] && literal == value[index - 1],
                };
            }
            previous = current;
        }
        previous[value.len()]
    }
}
