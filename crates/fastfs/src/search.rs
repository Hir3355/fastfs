use crate::native_matcher::{MatcherError, MatcherOptions, NativeMatcher};
use crate::native_scanner::{
    BinaryDetection, LineSink, NativeScanner, ScanControl, ScanLine, ScanMode, ScanResult,
    ScannerOptions,
};
use crate::native_walker::{NativeWalkOptions, NativeWalker, WalkControl, WalkError};
use crate::search_cache::{
    FileListCacheKey, begin as begin_cache, commit as commit_cache, lookup as lookup_cache,
};
use crate::search_platform::{
    open_search_file, roots_are_fast_storage, sequential_scan_is_beneficial,
    sequential_scan_size_probe_enabled,
};
use crate::wire::{Emitter, EncodedTextBatch};
use std::borrow::Cow;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{SyncSender, sync_channel},
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SearchOptions {
    line_number: bool,
    ignore_case: bool,
    smart_case: bool,
    fixed_strings: bool,
    word_regexp: bool,
    line_regexp: bool,
    before_context: usize,
    after_context: usize,
    max_count: Option<u64>,
    globs: Vec<String>,
    hidden: bool,
    no_ignore: bool,
    follow: bool,
    text: bool,
    files_with_matches: bool,
    count: bool,
}

struct SearchRequest {
    pattern: String,
    roots: Vec<PathBuf>,
    options: SearchOptions,
}

pub(crate) fn rg(args: &[String], emitter: &mut Emitter) {
    if emitter.is_stopped() {
        return;
    }
    let request = match parse_request(args) {
        Ok(request) => request,
        Err((code, message, target)) => {
            emitter.error(code, "InvalidArgument", message, target);
            return;
        }
    };

    let matcher = match build_matcher(&request.options, &request.pattern) {
        Ok(matcher) => matcher,
        Err(error) => {
            emitter.error(
                "InvalidPattern",
                "InvalidArgument",
                format!("rg の正規表現が不正です: {error}"),
                Some(request.pattern),
            );
            return;
        }
    };

    let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let walker = match NativeWalker::new(
        request.roots.clone(),
        NativeWalkOptions {
            hidden: request.options.hidden,
            no_ignore: request.options.no_ignore,
            follow_links: request.options.follow,
            globs: request.options.globs.clone(),
            current_dir: Some(current_dir.clone()),
        },
    ) {
        Ok(walker) => walker,
        Err(error) => {
            if let Some(pattern) = error.pattern() {
                emitter.error(
                    "InvalidGlob",
                    "InvalidArgument",
                    format!("rg のglobが不正です: {error}"),
                    Some(pattern.to_owned()),
                );
            } else {
                emitter.error(
                    "WalkFailed",
                    "ReadError",
                    format!("rg のファイル走査を初期化できませんでした: {error}"),
                    None,
                );
            }
            return;
        }
    };

    let include_path = request.roots.len() > 1 || request.roots.iter().any(|path| path.is_dir());

    let has_directory = request.roots.iter().any(|path| path.is_dir());
    let use_parallel = should_search_parallel(&request.roots, request.options.follow);
    if !use_parallel {
        let sequential_hint = !has_directory && use_sequential_hint(&request.options);
        search_sequential(
            &walker,
            &matcher,
            include_path,
            &request.options,
            sequential_hint,
            emitter,
        );
        return;
    }

    let thread_count = search_thread_count(&request.roots);
    let cache_key = FileListCacheKey::new(
        &request.roots,
        &current_dir,
        request.options.hidden,
        request.options.no_ignore,
        request.options.follow,
        &request.options.globs,
    );
    if let Some(paths) = cache_key.as_ref().and_then(lookup_cache) {
        search_paths(
            paths,
            matcher,
            include_path,
            request.options,
            thread_count,
            false,
            emitter,
        );
        return;
    }

    let cache_build = cache_key.and_then(begin_cache);
    let collected_paths = search_parallel_walk(
        walker,
        matcher,
        include_path,
        request.options,
        thread_count,
        cache_build.is_some(),
        emitter,
    );
    if let (Some(build), Some(paths)) = (cache_build, collected_paths) {
        commit_cache(build, paths);
    }
}

fn build_matcher(options: &SearchOptions, pattern: &str) -> Result<NativeMatcher, MatcherError> {
    NativeMatcher::build(
        pattern,
        MatcherOptions {
            ignore_case: options.ignore_case,
            smart_case: options.smart_case,
            fixed_strings: options.fixed_strings,
            word_regexp: options.word_regexp,
            line_regexp: options.line_regexp,
            text: options.text,
        },
    )
}

fn search_sequential(
    walker: &NativeWalker,
    matcher: &NativeMatcher,
    include_path: bool,
    options: &SearchOptions,
    sequential_hint: bool,
    emitter: &mut Emitter,
) {
    let mut scanner = build_scanner(options);
    let cancellation = emitter.cancellation();
    for entry in walker.build() {
        if emitter.is_stopped() {
            return;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                emitter.error(
                    "WalkFailed",
                    "ReadError",
                    format!("rg のファイル走査に失敗しました: {error}"),
                    walk_error_path(&error),
                );
                continue;
            }
        };
        search_file(
            &mut scanner,
            matcher,
            entry.filesystem_path(),
            entry.path(),
            include_path,
            options,
            sequential_hint,
            cancellation.as_deref(),
            emitter,
        );
    }
}

fn search_thread_count(roots: &[PathBuf]) -> usize {
    let logical = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    static PHYSICAL: OnceLock<usize> = OnceLock::new();
    let physical = (*PHYSICAL.get_or_init(num_cpus::get_physical)).clamp(1, logical);
    choose_thread_count(logical, physical, roots_are_fast_storage(roots))
}

fn should_search_parallel(roots: &[PathBuf], follow: bool) -> bool {
    const FILE_THRESHOLD: usize = 64;
    const ENTRY_THRESHOLD: usize = 256;

    let mut file_count = roots.iter().filter(|path| path.is_file()).count();
    if file_count > FILE_THRESHOLD || roots.len() > ENTRY_THRESHOLD {
        return true;
    }
    let mut pending = roots
        .iter()
        .filter(|path| path.is_dir())
        .cloned()
        .collect::<Vec<_>>();
    let mut entry_count = 0_usize;
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return true;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return true;
            };
            entry_count += 1;
            let Ok(kind) = entry.file_type() else {
                return true;
            };
            if kind.is_file() {
                file_count += 1;
            } else if kind.is_dir() {
                pending.push(entry.path());
            } else if follow && kind.is_symlink() {
                return true;
            }
            if file_count > FILE_THRESHOLD || entry_count > ENTRY_THRESHOLD {
                return true;
            }
        }
    }
    false
}

fn choose_thread_count(logical: usize, physical: usize, fast_storage: bool) -> usize {
    let logical = logical.max(1);
    let conservative = logical.min(12);
    if !fast_storage {
        return conservative;
    }
    logical.min(physical.clamp(1, logical).max(12)).min(20)
}

fn build_scanner(options: &SearchOptions) -> NativeScanner {
    NativeScanner::new(ScannerOptions {
        before_context: options.before_context,
        after_context: options.after_context,
        max_matches: options.max_count,
        binary_detection: if options.text {
            BinaryDetection::Text
        } else {
            BinaryDetection::Quit
        },
        mode: if options.count {
            ScanMode::Count
        } else if options.files_with_matches {
            ScanMode::FilesWithMatches
        } else {
            ScanMode::Standard
        },
    })
}

enum WorkerMessage {
    Output(EncodedTextBatch),
    Error {
        code: &'static str,
        message: String,
        path: Option<String>,
    },
}

type SequencedPaths = Arc<Mutex<Vec<(usize, PathBuf)>>>;

struct ThreadPathCollector {
    local: Vec<(usize, PathBuf)>,
    shared: Option<SequencedPaths>,
    next_sequence: Option<Arc<AtomicUsize>>,
}

impl ThreadPathCollector {
    fn new(shared: Option<SequencedPaths>, next_sequence: Option<Arc<AtomicUsize>>) -> Self {
        Self {
            local: Vec::new(),
            shared,
            next_sequence,
        }
    }

    fn push(&mut self, path: &Path) {
        if self.shared.is_some()
            && let Some(next_sequence) = &self.next_sequence
        {
            let sequence = next_sequence.fetch_add(1, Ordering::Relaxed);
            self.local.push((sequence, path.to_path_buf()));
        }
    }
}

impl Drop for ThreadPathCollector {
    fn drop(&mut self) {
        let Some(shared) = &self.shared else {
            return;
        };
        shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .append(&mut self.local);
    }
}

fn search_parallel_walk(
    walker: NativeWalker,
    matcher: NativeMatcher,
    include_path: bool,
    options: SearchOptions,
    thread_count: usize,
    collect_paths: bool,
    emitter: &mut Emitter,
) -> Option<Vec<PathBuf>> {
    let (sender, receiver) = sync_channel::<WorkerMessage>(24);
    let stopped = emitter
        .cancellation()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let output_gate = Arc::new(Mutex::new(()));
    let collected_paths = collect_paths.then(|| Arc::new(Mutex::new(Vec::new())));
    let next_path_sequence = collect_paths.then(|| Arc::new(AtomicUsize::new(0)));
    let cache_valid = Arc::new(AtomicBool::new(true));

    std::thread::scope(|scope| {
        let worker_stopped = Arc::clone(&stopped);
        let worker_gate = Arc::clone(&output_gate);
        let worker_paths = collected_paths.clone();
        let worker_path_sequence = next_path_sequence.clone();
        let worker_cache_valid = Arc::clone(&cache_valid);
        let worker = scope.spawn(move || {
            let matcher = Arc::new(matcher);
            let options = Arc::new(options);
            walker.run_parallel(thread_count, move || {
                let sender = sender.clone();
                let matcher = Arc::clone(&matcher);
                let options = Arc::clone(&options);
                let stopped = Arc::clone(&worker_stopped);
                let output_gate = Arc::clone(&worker_gate);
                let cache_valid = Arc::clone(&worker_cache_valid);
                let mut path_collector =
                    ThreadPathCollector::new(worker_paths.clone(), worker_path_sequence.clone());
                let mut scanner = build_scanner(&options);
                move |entry| {
                    if stopped.load(Ordering::Relaxed) {
                        return WalkControl::Quit;
                    }
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            cache_valid.store(false, Ordering::Relaxed);
                            return send_worker_message(
                                &sender,
                                WorkerMessage::Error {
                                    code: "WalkFailed",
                                    message: format!("rg のファイル走査に失敗しました: {error}"),
                                    path: walk_error_path(&error),
                                },
                            );
                        }
                    };
                    let display_path = entry.path();
                    let filesystem_path = entry.filesystem_path();
                    path_collector.push(display_path);
                    let sequential_hint = use_sequential_hint(&options)
                        && sequential_scan_size_probe_enabled()
                        && sequential_scan_is_beneficial(entry.len());
                    match search_file_streaming(
                        &mut scanner,
                        &matcher,
                        filesystem_path,
                        display_path,
                        include_path,
                        &options,
                        sequential_hint,
                        &sender,
                        &output_gate,
                        &stopped,
                    ) {
                        Ok(()) => WalkControl::Continue,
                        Err(_) if stopped.load(Ordering::Relaxed) => WalkControl::Quit,
                        Err(error) => send_worker_message(
                            &sender,
                            WorkerMessage::Error {
                                code: "SearchFailed",
                                message: format!("rg でファイルを検索できませんでした: {error}"),
                                path: Some(display_path.to_string_lossy().into_owned()),
                            },
                        ),
                    }
                }
            });
        });

        receive_worker_messages(receiver, &stopped, emitter);
        worker.join().expect("rg parallel search worker panicked");
    });

    if !collect_paths || stopped.load(Ordering::Relaxed) || !cache_valid.load(Ordering::Relaxed) {
        return None;
    }
    let paths = collected_paths?;
    let mut numbered_paths = Arc::try_unwrap(paths)
        .unwrap_or_else(|_| panic!("rg path collector still has owners"))
        .into_inner()
        .unwrap_or_else(|error| error.into_inner());
    numbered_paths.sort_unstable_by_key(|(sequence, _)| *sequence);
    Some(numbered_paths.into_iter().map(|(_, path)| path).collect())
}

fn search_paths(
    paths: Arc<[PathBuf]>,
    matcher: NativeMatcher,
    include_path: bool,
    options: SearchOptions,
    thread_count: usize,
    sequential_hint: bool,
    emitter: &mut Emitter,
) {
    if paths.len() <= 64 {
        let mut scanner = build_scanner(&options);
        let cancellation = emitter.cancellation();
        for path in paths.iter() {
            if emitter.is_stopped() {
                break;
            }
            search_file(
                &mut scanner,
                &matcher,
                path,
                path,
                include_path,
                &options,
                sequential_hint && use_sequential_hint(&options),
                cancellation.as_deref(),
                emitter,
            );
        }
        return;
    }

    let (sender, receiver) = sync_channel::<WorkerMessage>(24);
    let stopped = emitter
        .cancellation()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let output_gate = Arc::new(Mutex::new(()));
    let next = Arc::new(AtomicUsize::new(0));
    let matcher = Arc::new(matcher);
    let options = Arc::new(options);

    std::thread::scope(|scope| {
        for _ in 0..thread_count.min(paths.len()) {
            let sender = sender.clone();
            let stopped = Arc::clone(&stopped);
            let output_gate = Arc::clone(&output_gate);
            let next = Arc::clone(&next);
            let paths = Arc::clone(&paths);
            let matcher = Arc::clone(&matcher);
            let options = Arc::clone(&options);
            scope.spawn(move || {
                let mut scanner = build_scanner(&options);
                loop {
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(index) else {
                        break;
                    };
                    match search_file_streaming(
                        &mut scanner,
                        &matcher,
                        path,
                        path,
                        include_path,
                        &options,
                        sequential_hint && use_sequential_hint(&options),
                        &sender,
                        &output_gate,
                        &stopped,
                    ) {
                        Ok(()) => {}
                        Err(_) if stopped.load(Ordering::Relaxed) => break,
                        Err(error) => {
                            if send_worker_message(
                                &sender,
                                WorkerMessage::Error {
                                    code: "SearchFailed",
                                    message: format!(
                                        "rg でファイルを検索できませんでした: {error}"
                                    ),
                                    path: Some(path.to_string_lossy().into_owned()),
                                },
                            ) == WalkControl::Quit
                            {
                                break;
                            }
                        }
                    }
                }
            });
        }
        drop(sender);
        receive_worker_messages(receiver, &stopped, emitter);
    });
}

fn receive_worker_messages(
    receiver: std::sync::mpsc::Receiver<WorkerMessage>,
    stopped: &AtomicBool,
    emitter: &mut Emitter,
) {
    while let Ok(message) = receiver.recv() {
        match message {
            WorkerMessage::Output(batch) => {
                if !emitter.text_batch(batch) {
                    stopped.store(true, Ordering::Relaxed);
                    break;
                }
            }
            WorkerMessage::Error {
                code,
                message,
                path,
            } => {
                emitter.error(code, "ReadError", message, path);
                if emitter.is_stopped() {
                    stopped.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    }
}

fn send_worker_message(sender: &SyncSender<WorkerMessage>, message: WorkerMessage) -> WalkControl {
    if sender.send(message).is_ok() {
        WalkControl::Continue
    } else {
        WalkControl::Quit
    }
}

fn walk_error_path(error: &WalkError) -> Option<String> {
    error.path().map(|path| path.to_string_lossy().into_owned())
}

#[allow(clippy::too_many_arguments)]
fn search_file(
    scanner: &mut NativeScanner,
    matcher: &NativeMatcher,
    filesystem_path: &Path,
    display_path: &Path,
    include_path: bool,
    options: &SearchOptions,
    sequential_hint: bool,
    cancelled: Option<&AtomicBool>,
    emitter: &mut Emitter,
) {
    let mut sink = FastFsSearchSink {
        emitter,
        path: display_path,
        include_path,
        line_number: options.line_number,
        files_with_matches: options.files_with_matches,
        count_only: options.count,
    };
    let result = match execute_search(
        scanner,
        matcher,
        filesystem_path,
        &mut sink,
        cancelled,
        sequential_hint,
    ) {
        Ok(result) => result,
        Err(error) => {
            sink.emitter.error(
                "SearchFailed",
                "ReadError",
                format!("rg でファイルを検索できませんでした: {error}"),
                Some(display_path.to_string_lossy().into_owned()),
            );
            return;
        }
    };
    if result.cancelled {
        sink.emitter.is_stopped();
        return;
    }
    if options.count && result.match_count > 0 && !sink.emitter.is_stopped() {
        let match_count = result.match_count;
        let value = if include_path {
            format!("{}:{match_count}", display_path.to_string_lossy())
        } else {
            match_count.to_string()
        };
        sink.emitter.text(&value);
    }
}

#[allow(clippy::too_many_arguments)]
fn search_file_streaming(
    scanner: &mut NativeScanner,
    matcher: &NativeMatcher,
    filesystem_path: &Path,
    display_path: &Path,
    include_path: bool,
    options: &SearchOptions,
    sequential_hint: bool,
    sender: &SyncSender<WorkerMessage>,
    output_gate: &Mutex<()>,
    stopped: &AtomicBool,
) -> io::Result<()> {
    let mut sink = StreamingSearchSink {
        path: display_path,
        include_path,
        line_number: options.line_number,
        files_with_matches: options.files_with_matches,
        count_only: options.count,
        batch: EncodedTextBatch::new(),
        batch_limit: 16,
        sender,
        output_gate,
        output_guard: None,
        stopped,
    };
    let search_result = execute_search(
        scanner,
        matcher,
        filesystem_path,
        &mut sink,
        Some(stopped),
        sequential_hint,
    );
    let search_result = match search_result {
        Ok(result) => result,
        Err(error) => return flush_streaming_output_before_error(&mut sink, error),
    };
    if options.count && search_result.match_count > 0 && !stopped.load(Ordering::Relaxed) {
        let value = if include_path {
            format!(
                "{}:{}",
                display_path.to_string_lossy(),
                search_result.match_count
            )
        } else {
            search_result.match_count.to_string()
        };
        if !sink.push_text(&value)? {
            return Ok(());
        }
    }
    sink.flush()?;
    Ok(())
}

fn flush_streaming_output_before_error(
    sink: &mut StreamingSearchSink<'_>,
    error: io::Error,
) -> io::Result<()> {
    let flush_result = sink.flush();
    if sink.stopped.load(Ordering::Relaxed) {
        return Ok(());
    }
    flush_result?;
    Err(error)
}

fn use_sequential_hint(options: &SearchOptions) -> bool {
    !options.files_with_matches && options.max_count.is_none()
}

fn execute_search<S>(
    scanner: &mut NativeScanner,
    matcher: &NativeMatcher,
    path: &Path,
    sink: &mut S,
    stopped: Option<&AtomicBool>,
    sequential_hint: bool,
) -> io::Result<ScanResult>
where
    S: LineSink,
{
    let file = open_search_file(path, sequential_hint)?;
    scanner.scan_reader(file, matcher, sink, stopped)
}

struct StreamingSearchSink<'a> {
    path: &'a Path,
    include_path: bool,
    line_number: bool,
    files_with_matches: bool,
    count_only: bool,
    batch: EncodedTextBatch,
    batch_limit: u32,
    sender: &'a SyncSender<WorkerMessage>,
    output_gate: &'a Mutex<()>,
    output_guard: Option<MutexGuard<'a, ()>>,
    stopped: &'a AtomicBool,
}

impl LineSink for StreamingSearchSink<'_> {
    fn matched(&mut self, matched: ScanLine<'_>) -> io::Result<ScanControl> {
        if self.stopped.load(Ordering::Relaxed) {
            return Ok(ScanControl::Stop);
        }
        if self.files_with_matches {
            let path = self.path.to_string_lossy().into_owned();
            self.push_text(&path)?;
            return Ok(ScanControl::Stop);
        }
        if !self.count_only {
            let line = format_search_line(
                self.path,
                self.include_path,
                self.line_number,
                matched.bytes,
                matched.line_number,
                ':',
            );
            if !self.push_text(&line)? {
                return Ok(ScanControl::Stop);
            }
        }
        Ok(ScanControl::Continue)
    }

    fn context(&mut self, context: ScanLine<'_>) -> io::Result<ScanControl> {
        if self.stopped.load(Ordering::Relaxed) {
            return Ok(ScanControl::Stop);
        }
        if !self.files_with_matches && !self.count_only {
            let line = format_search_line(
                self.path,
                self.include_path,
                self.line_number,
                context.bytes,
                context.line_number,
                '-',
            );
            if !self.push_text(&line)? {
                return Ok(ScanControl::Stop);
            }
        }
        Ok(ScanControl::Continue)
    }

    fn context_break(&mut self) -> io::Result<ScanControl> {
        if self.stopped.load(Ordering::Relaxed) {
            return Ok(ScanControl::Stop);
        }
        if !self.files_with_matches && !self.count_only {
            return Ok(if self.push_text("--")? {
                ScanControl::Continue
            } else {
                ScanControl::Stop
            });
        }
        Ok(ScanControl::Continue)
    }
}

impl StreamingSearchSink<'_> {
    fn push_text(&mut self, value: &str) -> io::Result<bool> {
        if self.stopped.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if !self.batch.push(value) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rg の出力行が大きすぎます",
            ));
        }
        if self.batch.count() >= self.batch_limit || self.batch.len() >= 64 * 1024 {
            self.flush()?;
        }
        Ok(!self.stopped.load(Ordering::Relaxed))
    }

    fn acquire_output_gate(&mut self) -> bool {
        if self.output_guard.is_none() {
            let guard = self
                .output_gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if self.stopped.load(Ordering::Relaxed) {
                return false;
            }
            self.output_guard = Some(guard);
        }
        true
    }

    fn flush(&mut self) -> io::Result<bool> {
        if self.batch.is_empty() {
            return Ok(!self.stopped.load(Ordering::Relaxed));
        }
        if self.stopped.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if !self.acquire_output_gate() {
            return Ok(false);
        }
        let emitted_count = self.batch.count();
        let batch = std::mem::take(&mut self.batch);
        if self.sender.send(WorkerMessage::Output(batch)).is_err() {
            self.stopped.store(true, Ordering::Relaxed);
            return Err(io::Error::other("rg output receiver closed"));
        }
        if emitted_count >= self.batch_limit {
            self.batch_limit = (self.batch_limit * 2).min(256);
        }
        Ok(true)
    }
}

fn format_search_line(
    path: &Path,
    include_path: bool,
    line_number_enabled: bool,
    bytes: &[u8],
    line_number: u64,
    separator: char,
) -> String {
    let text = decode_line(bytes);
    if !include_path && !line_number_enabled {
        return text.into_owned();
    }
    let path = path.to_string_lossy();
    let mut output = String::with_capacity(path.len() + text.len() + 24);
    if include_path {
        output.push_str(&path);
    }
    if line_number_enabled {
        if include_path {
            output.push(separator);
        }
        let _ = write!(output, "{line_number}");
    }
    output.push(separator);
    output.push_str(&text);
    output
}

struct FastFsSearchSink<'a> {
    emitter: &'a mut Emitter,
    path: &'a Path,
    include_path: bool,
    line_number: bool,
    files_with_matches: bool,
    count_only: bool,
}

impl LineSink for FastFsSearchSink<'_> {
    fn matched(&mut self, matched: ScanLine<'_>) -> io::Result<ScanControl> {
        if self.files_with_matches {
            self.emitter.text(&self.path.to_string_lossy());
            return Ok(ScanControl::Stop);
        }
        if self.count_only {
            return Ok(if self.emitter.is_stopped() {
                ScanControl::Stop
            } else {
                ScanControl::Continue
            });
        }
        Ok(if self.emit_line(matched.bytes, matched.line_number, ':') {
            ScanControl::Continue
        } else {
            ScanControl::Stop
        })
    }

    fn context(&mut self, context: ScanLine<'_>) -> io::Result<ScanControl> {
        if self.files_with_matches || self.count_only {
            return Ok(if self.emitter.is_stopped() {
                ScanControl::Stop
            } else {
                ScanControl::Continue
            });
        }
        Ok(if self.emit_line(context.bytes, context.line_number, '-') {
            ScanControl::Continue
        } else {
            ScanControl::Stop
        })
    }

    fn context_break(&mut self) -> io::Result<ScanControl> {
        if self.files_with_matches || self.count_only {
            return Ok(if self.emitter.is_stopped() {
                ScanControl::Stop
            } else {
                ScanControl::Continue
            });
        }
        Ok(if self.emitter.text("--") {
            ScanControl::Continue
        } else {
            ScanControl::Stop
        })
    }
}

impl FastFsSearchSink<'_> {
    fn emit_line(&mut self, bytes: &[u8], line_number: u64, separator: char) -> bool {
        let text = decode_line(bytes);
        if !self.include_path && !self.line_number {
            return self.emitter.text(&text);
        }

        let path = self.path.to_string_lossy();
        let mut output = String::with_capacity(path.len() + text.len() + 24);
        if self.include_path {
            output.push_str(&path);
        }
        if self.line_number {
            if self.include_path {
                output.push(separator);
            }
            let _ = write!(output, "{line_number}");
        }
        output.push(separator);
        output.push_str(&text);
        self.emitter.text(&output)
    }
}

fn decode_line(mut bytes: &[u8]) -> Cow<'_, str> {
    if bytes.last() == Some(&b'\n') {
        bytes = &bytes[..bytes.len() - 1];
        if bytes.last() == Some(&b'\r') {
            bytes = &bytes[..bytes.len() - 1];
        }
    }
    String::from_utf8_lossy(bytes)
}

type ParseError = (&'static str, String, Option<String>);

fn parse_request(args: &[String]) -> Result<SearchRequest, ParseError> {
    let mut options = SearchOptions::default();
    let mut positional = Vec::new();
    let mut options_ended = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if options_ended {
            positional.push(arg.clone());
        } else {
            match arg.as_str() {
                "--" => options_ended = true,
                "-n" | "--line-number" => options.line_number = true,
                "-i" | "--ignore-case" => options.ignore_case = true,
                "-S" | "--smart-case" => options.smart_case = true,
                "-F" | "--fixed-strings" => options.fixed_strings = true,
                "-w" | "--word-regexp" => options.word_regexp = true,
                "-x" | "--line-regexp" => options.line_regexp = true,
                "--hidden" => options.hidden = true,
                "--no-ignore" => options.no_ignore = true,
                "-L" | "--follow" => options.follow = true,
                "-a" | "--text" => options.text = true,
                "-l" | "--files-with-matches" => options.files_with_matches = true,
                "-c" | "--count" => options.count = true,
                "-e" | "--regexp" => {
                    positional.push(next_value(args, &mut index, arg)?.clone());
                }
                "-C" | "--context" => {
                    let value = next_value(args, &mut index, arg)?;
                    let count = parse_usize(value, arg)?;
                    options.before_context = count;
                    options.after_context = count;
                }
                "-A" | "--after-context" => {
                    let value = next_value(args, &mut index, arg)?;
                    options.after_context = parse_usize(value, arg)?;
                }
                "-B" | "--before-context" => {
                    let value = next_value(args, &mut index, arg)?;
                    options.before_context = parse_usize(value, arg)?;
                }
                "-m" | "--max-count" => {
                    let value = next_value(args, &mut index, arg)?;
                    options.max_count = Some(parse_u64(value, arg)?);
                }
                "-g" | "--glob" => {
                    options
                        .globs
                        .push(next_value(args, &mut index, arg)?.clone());
                }
                value if value.starts_with('-') => {
                    return Err((
                        "InvalidOption",
                        format!("rg の不明なオプションです: {value}"),
                        Some(value.to_owned()),
                    ));
                }
                _ => positional.push(arg.clone()),
            }
        }
        index += 1;
    }

    if options.files_with_matches && options.count {
        return Err((
            "ConflictingOptions",
            "rg では --files-with-matches と --count を同時に指定できません".to_owned(),
            None,
        ));
    }
    let Some(pattern) = positional.first().cloned() else {
        return Err((
            "MissingPattern",
            "rg には検索パターンが必要です".to_owned(),
            None,
        ));
    };
    let roots = if positional.len() == 1 {
        vec![PathBuf::from(".")]
    } else {
        positional[1..].iter().map(PathBuf::from).collect()
    };
    Ok(SearchRequest {
        pattern,
        roots,
        options,
    })
}

fn next_value<'a>(
    args: &'a [String],
    index: &mut usize,
    option: &str,
) -> Result<&'a String, ParseError> {
    *index += 1;
    args.get(*index).ok_or_else(|| {
        (
            "MissingOptionValue",
            format!("rg {option} には値が必要です"),
            Some(option.to_owned()),
        )
    })
}

fn parse_usize(value: &str, option: &str) -> Result<usize, ParseError> {
    value.parse().map_err(|_| {
        (
            "InvalidOptionValue",
            format!("rg {option} の値は0以上の整数で指定してください: {value}"),
            Some(value.to_owned()),
        )
    })
}

fn parse_u64(value: &str, option: &str) -> Result<u64, ParseError> {
    value.parse().map_err(|_| {
        (
            "InvalidOptionValue",
            format!("rg {option} の値は0以上の整数で指定してください: {value}"),
            Some(value.to_owned()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        SearchOptions, StreamingSearchSink, WorkerMessage, build_matcher, choose_thread_count,
        decode_line, flush_streaming_output_before_error, parse_request, send_worker_message,
        should_search_parallel,
    };
    use crate::native_scanner::{LineSink, NativeScanner, ScanControl, ScanLine, ScannerOptions};
    use crate::native_walker::WalkControl;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, mpsc::sync_channel};

    #[derive(Default)]
    struct TestSink {
        matches: Vec<(u64, String)>,
    }

    impl LineSink for TestSink {
        fn matched(&mut self, line: ScanLine<'_>) -> io::Result<ScanControl> {
            self.matches.push((
                line.line_number,
                String::from_utf8_lossy(line.bytes).into_owned(),
            ));
            Ok(ScanControl::Continue)
        }

        fn context(&mut self, _line: ScanLine<'_>) -> io::Result<ScanControl> {
            Ok(ScanControl::Continue)
        }

        fn context_break(&mut self) -> io::Result<ScanControl> {
            Ok(ScanControl::Continue)
        }
    }

    #[test]
    fn parses_codex_search_options() {
        let args = [
            "needle", ".", "-n", "-C", "10", "-g", "*.rs", "-S", "-m", "5",
        ]
        .map(str::to_owned);
        let request = parse_request(&args).unwrap();
        assert_eq!(request.pattern, "needle");
        assert_eq!(request.roots, [PathBuf::from(".")]);
        assert_eq!(
            request.options,
            SearchOptions {
                line_number: true,
                smart_case: true,
                before_context: 10,
                after_context: 10,
                max_count: Some(5),
                globs: vec!["*.rs".to_owned()],
                ..SearchOptions::default()
            }
        );
    }

    #[test]
    fn defaults_to_current_directory() {
        let request = parse_request(&["needle".to_owned()]).unwrap();
        assert_eq!(request.roots, [PathBuf::from(".")]);
    }

    #[test]
    fn rejects_missing_values_and_conflicting_modes() {
        assert!(parse_request(&["needle".to_owned(), "-C".to_owned()]).is_err());
        assert!(parse_request(&["needle".to_owned(), "-l".to_owned(), "-c".to_owned(),]).is_err());
    }

    #[test]
    fn decodes_crlf_and_invalid_utf8_lossily() {
        assert_eq!(decode_line(b"text\r\n"), "text");
        assert_eq!(decode_line(b"a\xFF\n"), "a\u{FFFD}");
    }

    #[test]
    fn matcher_uses_line_boundaries_and_bans_nul_for_binary_search() {
        let options = SearchOptions::default();
        let matcher = build_matcher(&options, r"^Needle$").unwrap();
        let mut scanner = NativeScanner::new(ScannerOptions::default());
        let mut sink = TestSink::default();
        scanner
            .scan_bytes(b"prefix\nNeedle\nsuffix\n", &matcher, &mut sink, None)
            .unwrap();
        assert_eq!(sink.matches, [(2, "Needle".to_owned())]);
        assert!(build_matcher(&options, r"\x00").is_err());
        assert!(
            build_matcher(
                &SearchOptions {
                    text: true,
                    ..SearchOptions::default()
                },
                r"\x00",
            )
            .is_ok()
        );
    }

    #[test]
    fn adaptive_thread_count_is_conservative_for_unknown_storage() {
        assert_eq!(choose_thread_count(28, 20, true), 20);
        assert_eq!(choose_thread_count(16, 8, true), 12);
        assert_eq!(choose_thread_count(8, 4, true), 8);
        assert_eq!(choose_thread_count(28, 20, false), 12);
        assert_eq!(choose_thread_count(4, 16, true), 4);
    }

    #[test]
    fn bounded_metadata_probe_selects_parallel_at_file_threshold() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fastfs-parallel-probe-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        for index in 0..64 {
            std::fs::write(root.join(format!("{index}.txt")), b"").unwrap();
        }
        assert!(!should_search_parallel(std::slice::from_ref(&root), false));
        std::fs::write(root.join("64.txt"), b"").unwrap();
        assert!(should_search_parallel(std::slice::from_ref(&root), false));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn streaming_sink_emits_progressive_chunks() {
        let (sender, receiver) = sync_channel(24);
        let gate = Mutex::new(());
        let stopped = AtomicBool::new(false);
        let mut sink = StreamingSearchSink {
            path: Path::new("sample.txt"),
            include_path: true,
            line_number: true,
            files_with_matches: false,
            count_only: false,
            batch: Default::default(),
            batch_limit: 16,
            sender: &sender,
            output_gate: &gate,
            output_guard: None,
            stopped: &stopped,
        };
        for index in 0..700 {
            assert!(
                sink.push_text(&format!("sample.txt:{}:match", index + 1))
                    .unwrap()
            );
            if index == 0 {
                assert!(
                    gate.try_lock().is_ok(),
                    "output gate should remain available until the first batch is sent"
                );
            }
        }
        assert!(sink.flush().unwrap());
        drop(sink);
        drop(sender);

        let counts = receiver
            .into_iter()
            .map(|message| match message {
                WorkerMessage::Output(batch) => batch.count(),
                WorkerMessage::Error { .. } => panic!("unexpected error batch"),
            })
            .collect::<Vec<_>>();
        assert_eq!(counts, [16, 32, 64, 128, 256, 204]);
        assert!(!stopped.load(Ordering::Relaxed));
    }

    #[test]
    fn streaming_error_flushes_partial_output_before_error_message() {
        let (sender, receiver) = sync_channel(24);
        let gate = Mutex::new(());
        let stopped = AtomicBool::new(false);
        let mut sink = StreamingSearchSink {
            path: Path::new("sample.txt"),
            include_path: true,
            line_number: true,
            files_with_matches: false,
            count_only: false,
            batch: Default::default(),
            batch_limit: 16,
            sender: &sender,
            output_gate: &gate,
            output_guard: None,
            stopped: &stopped,
        };
        assert!(sink.push_text("sample.txt:1:partial").unwrap());

        let error = flush_streaming_output_before_error(
            &mut sink,
            io::Error::other("read failed after partial output"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "read failed after partial output");
        assert_eq!(
            send_worker_message(
                &sender,
                WorkerMessage::Error {
                    code: "SearchFailed",
                    message: error.to_string(),
                    path: Some("sample.txt".to_owned()),
                },
            ),
            WalkControl::Continue,
        );
        drop(sink);
        drop(sender);

        let messages = receiver.into_iter().collect::<Vec<_>>();
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[0], WorkerMessage::Output(_)));
        assert!(matches!(messages[1], WorkerMessage::Error { .. }));
    }

    #[test]
    fn streaming_error_discards_pending_output_after_cancellation() {
        let (sender, receiver) = sync_channel(24);
        let gate = Mutex::new(());
        let stopped = AtomicBool::new(false);
        let mut sink = StreamingSearchSink {
            path: Path::new("sample.txt"),
            include_path: true,
            line_number: true,
            files_with_matches: false,
            count_only: false,
            batch: Default::default(),
            batch_limit: 16,
            sender: &sender,
            output_gate: &gate,
            output_guard: None,
            stopped: &stopped,
        };
        assert!(sink.push_text("sample.txt:1:partial").unwrap());
        stopped.store(true, Ordering::Relaxed);

        assert!(
            flush_streaming_output_before_error(&mut sink, io::Error::other("cancelled")).is_ok()
        );
        drop(sink);
        drop(sender);
        assert!(receiver.into_iter().next().is_none());
    }

    #[test]
    fn scanner_observes_cancellation_before_searching() {
        let stopped = AtomicBool::new(true);
        let matcher = build_matcher(&SearchOptions::default(), "content").unwrap();
        let mut scanner = NativeScanner::new(ScannerOptions::default());
        let mut sink = TestSink::default();
        let result = scanner
            .scan_bytes(b"content", &matcher, &mut sink, Some(&stopped))
            .unwrap();
        assert!(result.cancelled);
        assert!(sink.matches.is_empty());
    }
}
