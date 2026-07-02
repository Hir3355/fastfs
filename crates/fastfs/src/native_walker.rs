//! FastFs's filesystem walker.
//!
//! This module deliberately owns its traversal, ignore-file and glob handling.
//! It does not share source or runtime code with another search tool. The public
//! surface is intentionally small: callers either pull entries with `build`,
//! or hand each worker a reusable visitor through `run_parallel`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, DirEntry};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};

const FILE_BATCH_SIZE: usize = 64;
const MAX_BUFFERED_DIRECTORY_ENTRIES: usize = 4096;
const MAX_BRACE_EXPANSIONS: usize = 256;
const MAX_USER_GLOB_BYTES: usize = 16 * 1024;
const MAX_GENERAL_GLOB_BYTES: usize = 1024;
const MAX_GLOB_MEMO_STATES: usize = 256 * 1024;
const MAX_IGNORE_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Options that affect recursive file discovery.
#[derive(Clone, Debug, Default)]
pub(crate) struct NativeWalkOptions {
    /// Include dot-prefixed names. A positive user glob also includes a hidden
    /// matching path, mirroring the useful `rg -g '*.rs'` behaviour.
    pub(crate) hidden: bool,
    /// Do not read `.rgignore`, `.ignore`, `.gitignore`, Git exclude files, or
    /// global Git excludes. This intentionally does not change hidden policy.
    pub(crate) no_ignore: bool,
    /// Resolve and traverse symbolic links. Disabled by default.
    pub(crate) follow_links: bool,
    /// Ordered `-g` patterns. A leading `!` excludes; all other patterns
    /// include and take precedence over hidden and ignore filtering.
    pub(crate) globs: Vec<String>,
    /// Base used for slash-containing `-g` patterns. Empty means the process
    /// working directory when the walker is created.
    pub(crate) current_dir: Option<PathBuf>,
}

/// A parsed walk configuration. Constructing it validates command-line globs;
/// ignore-file patterns are deliberately permissive, like Git's patterns.
#[derive(Clone)]
pub(crate) struct NativeWalker {
    roots: Arc<[RootSpec]>,
    options: NativeWalkOptions,
    current_dir: PathBuf,
    user_globs: Arc<UserGlobSet>,
    global_rules: Arc<[RuleTemplate]>,
}

#[derive(Clone)]
struct RootSpec {
    /// Normalized only for I/O and ignore matching. Keeping it separate from
    /// `display_path` preserves the caller's `.` / relative-root spelling.
    traversal_path: PathBuf,
    display_path: PathBuf,
}

impl NativeWalker {
    pub(crate) fn new(roots: Vec<PathBuf>, options: NativeWalkOptions) -> Result<Self, GlobError> {
        let current_dir = match options.current_dir.as_deref() {
            Some(path) => absolute_path(path).map_err(|error| {
                GlobError::new(
                    None,
                    format!("glob の基準ディレクトリを解決できませんでした: {error}"),
                )
            })?,
            None => env::current_dir().map_err(|error| {
                GlobError::new(
                    None,
                    format!("現在のディレクトリを取得できませんでした: {error}"),
                )
            })?,
        };
        let roots = if roots.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            roots
        };
        let mut root_specs = Vec::with_capacity(roots.len());
        for root in roots {
            let traversal_path = absolute_path_from(&current_dir, &root).map_err(|error| {
                GlobError::new(
                    None,
                    format!(
                        "検索ルートを解決できませんでした ({}): {error}",
                        root.display()
                    ),
                )
            })?;
            root_specs.push(RootSpec {
                traversal_path,
                display_path: root,
            });
        }

        let user_globs = UserGlobSet::parse(&options.globs)?;
        let global_rules = if options.no_ignore {
            Vec::new()
        } else {
            load_global_rule_templates()
        };

        Ok(Self {
            roots: root_specs.into(),
            options,
            current_dir,
            user_globs: Arc::new(user_globs),
            global_rules: global_rules.into(),
        })
    }

    /// Create a pull iterator. The iterator only emits regular files and
    /// reports filesystem failures as `WalkError` values.
    pub(crate) fn build(&self) -> NativeWalk {
        let runtime = WalkRuntime::new(self.options.follow_links);
        let initial = self.initial_work(&runtime);
        NativeWalk {
            walker: self.clone(),
            runtime,
            pending: initial.into(),
            current_batch: VecDeque::new(),
        }
    }

    /// Run a shared walk with one visitor instance per worker. A visitor is
    /// always called from the worker that created it, making scanner buffers
    /// and matchers safe to reuse without locking. Returning `Quit` stops the
    /// complete traversal as soon as the workers observe it.
    pub(crate) fn run_parallel<F, V>(&self, thread_count: usize, factory: F)
    where
        F: Fn() -> V + Sync,
        V: FnMut(Result<WalkEntry, WalkError>) -> WalkControl + Send,
    {
        let runtime = WalkRuntime::new(self.options.follow_links);
        let queue = Arc::new(WorkQueue::new(self.initial_work(&runtime)));
        let workers = thread_count.max(1);

        std::thread::scope(|scope| {
            for _ in 0..workers {
                let queue = Arc::clone(&queue);
                let walker = self.clone();
                let runtime = runtime.clone();
                let make_visitor = &factory;
                scope.spawn(move || {
                    let mut visitor = make_visitor();
                    worker_loop(&walker, &runtime, &queue, &mut visitor);
                });
            }
        });
    }

    fn initial_work(&self, runtime: &WalkRuntime) -> Vec<WorkItem> {
        let mut work = Vec::new();
        for root in self.roots.iter() {
            let root_base = root
                .traversal_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root.traversal_path.clone());
            let metadata = match fs::symlink_metadata(&root.traversal_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    work.push(WorkItem::Error(WalkError::io(
                        Some(root.display_path.clone()),
                        error,
                    )));
                    continue;
                }
            };
            let root_dir = if metadata.is_dir() {
                root.traversal_path.clone()
            } else {
                root_base.clone()
            };

            let (context, context_errors) = self.initial_context(&root_dir);
            work.extend(context_errors.into_iter().map(WorkItem::Error));
            if metadata.file_type().is_symlink() {
                if !self.options.follow_links {
                    continue;
                }
                match fs::metadata(&root.traversal_path) {
                    Ok(target) if target.is_file() => {
                        work.push(WorkItem::Files(vec![WalkEntry::new(
                            root.display_path.clone(),
                            root.traversal_path.clone(),
                            target.len(),
                        )]));
                    }
                    Ok(target) if target.is_dir() => {
                        if register_directory(runtime, &root.traversal_path, &root.display_path)
                            .unwrap_or_else(|error| {
                                work.push(WorkItem::Error(error));
                                false
                            })
                        {
                            work.push(WorkItem::Directory(DirectoryTask {
                                path: root.traversal_path.clone(),
                                display_path: root.display_path.clone(),
                                context,
                                root_base,
                            }));
                        }
                    }
                    Ok(_) => {}
                    Err(error) => work.push(WorkItem::Error(WalkError::io(
                        Some(root.display_path.clone()),
                        error,
                    ))),
                }
                continue;
            }

            if metadata.is_file() {
                work.push(WorkItem::Files(vec![WalkEntry::new(
                    root.display_path.clone(),
                    root.traversal_path.clone(),
                    metadata.len(),
                )]));
            } else if metadata.is_dir()
                && register_directory(runtime, &root.traversal_path, &root.display_path)
                    .unwrap_or_else(|error| {
                        work.push(WorkItem::Error(error));
                        false
                    })
            {
                work.push(WorkItem::Directory(DirectoryTask {
                    path: root.traversal_path.clone(),
                    display_path: root.display_path.clone(),
                    context,
                    root_base,
                }));
            }
        }
        work
    }

    fn initial_context(&self, root_dir: &Path) -> (RuleChain, Vec<WalkError>) {
        if self.options.no_ignore {
            return (RuleChain::empty(), Vec::new());
        }

        let mut context = RuleChain::empty();
        if !self.global_rules.is_empty() {
            context = context.extend(IgnoreLayer::from_templates(
                root_dir.to_path_buf(),
                Arc::clone(&self.global_rules),
            ));
        }

        let mut errors = Vec::new();
        let mut ancestors = Vec::new();
        let mut cursor = root_dir.parent();
        while let Some(directory) = cursor {
            ancestors.push(directory.to_path_buf());
            cursor = directory.parent();
        }
        ancestors.reverse();
        for directory in ancestors {
            let (next, mut layer_errors) =
                self.extend_context_for_directory(context, &directory, None);
            context = next;
            errors.append(&mut layer_errors);
        }
        // An explicitly supplied directory remains searchable even when an
        // ancestor rule excludes that directory as a whole. Direct matches
        // below the root (for example `*.log`) still retain their effect.
        context = context.with_explicit_root(root_dir.to_path_buf());
        (context, errors)
    }

    fn extend_context_for_directory(
        &self,
        context: RuleChain,
        directory: &Path,
        present: Option<&IgnoreFilePresence>,
    ) -> (RuleChain, Vec<WalkError>) {
        if self.options.no_ignore {
            return (context, Vec::new());
        }

        let mut context = context;
        let mut errors = Vec::new();

        if present.is_none_or(|present| present.dot_git) {
            for exclude in git_info_exclude_paths(directory) {
                match load_ignore_layer(&exclude, directory) {
                    Ok(Some(layer)) => context = context.extend(layer),
                    Ok(None) => {}
                    Err(error) => errors.push(error),
                }
            }
        }
        // Later layers win: Git rules < generic ignore < rg-specific ignore.
        for name in [".gitignore", ".ignore", ".rgignore"] {
            if present.is_some_and(|present| !present.contains(name)) {
                continue;
            }
            let path = directory.join(name);
            match load_ignore_layer(&path, directory) {
                Ok(Some(layer)) => context = context.extend(layer),
                Ok(None) => {}
                Err(error) => errors.push(error),
            }
        }
        (context, errors)
    }
}

/// The pull-style native walker returned by [`NativeWalker::build`].
pub(crate) struct NativeWalk {
    walker: NativeWalker,
    runtime: WalkRuntime,
    pending: VecDeque<WorkItem>,
    current_batch: VecDeque<WalkEntry>,
}

impl Iterator for NativeWalk {
    type Item = Result<WalkEntry, WalkError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(entry) = self.current_batch.pop_front() {
                return Some(Ok(entry));
            }
            let item = self.pending.pop_front()?;
            match item {
                WorkItem::Error(error) => return Some(Err(error)),
                WorkItem::Files(entries) => self.current_batch = entries.into(),
                WorkItem::Directory(task) => {
                    let produced = expand_directory(&self.walker, &self.runtime, task);
                    self.pending.extend(produced);
                }
            }
        }
    }
}

/// File data emitted by the walker. Only files are emitted; directories are
/// traversal work rather than public entries.
#[derive(Debug)]
pub(crate) struct WalkEntry {
    path: PathBuf,
    filesystem_path: PathBuf,
    length: u64,
}

impl WalkEntry {
    fn new(path: PathBuf, filesystem_path: PathBuf, length: u64) -> Self {
        Self {
            path,
            filesystem_path,
            length,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Absolute I/O path retained for callers that need to open the file
    /// without depending on the PowerShell process working directory.
    pub(crate) fn filesystem_path(&self) -> &Path {
        &self.filesystem_path
    }

    pub(crate) fn len(&self) -> u64 {
        self.length
    }
}

/// A filesystem or ignore-file error associated with the best known path.
#[derive(Debug)]
pub(crate) struct WalkError {
    path: Option<PathBuf>,
    message: String,
}

impl WalkError {
    fn io(path: Option<PathBuf>, error: io::Error) -> Self {
        Self {
            path,
            message: error.to_string(),
        }
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl fmt::Display for WalkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(formatter, "{}: {}", path.display(), self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

impl Error for WalkError {}

/// Error returned when a command-line `-g` pattern cannot be parsed.
#[derive(Debug, Clone)]
pub(crate) struct GlobError {
    pattern: Option<String>,
    message: String,
}

impl GlobError {
    fn new(pattern: Option<String>, message: impl Into<String>) -> Self {
        Self {
            pattern,
            message: message.into(),
        }
    }

    pub(crate) fn pattern(&self) -> Option<&str> {
        self.pattern.as_deref()
    }
}

impl fmt::Display for GlobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for GlobError {}

/// Return value of a parallel visitor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WalkControl {
    Continue,
    Quit,
}

#[derive(Clone)]
struct WalkRuntime {
    visited_directories: Option<Arc<Mutex<HashSet<PathBuf>>>>,
}

impl WalkRuntime {
    fn new(follow_links: bool) -> Self {
        Self {
            visited_directories: follow_links.then(|| Arc::new(Mutex::new(HashSet::new()))),
        }
    }
}

#[derive(Clone)]
struct DirectoryTask {
    path: PathBuf,
    display_path: PathBuf,
    /// Rules inherited from the parent. The task's own directory rules are
    /// loaded immediately before its children are evaluated.
    context: RuleChain,
    root_base: PathBuf,
}

#[derive(Default)]
struct IgnoreFilePresence {
    dot_git: bool,
    gitignore: bool,
    ignore: bool,
    rgignore: bool,
}

impl IgnoreFilePresence {
    fn observe(&mut self, name: &OsStr) {
        let Some(name) = name.to_str() else {
            return;
        };
        if name.eq_ignore_ascii_case(".git") {
            self.dot_git = true;
        } else if name.eq_ignore_ascii_case(".gitignore") {
            self.gitignore = true;
        } else if name.eq_ignore_ascii_case(".ignore") {
            self.ignore = true;
        } else if name.eq_ignore_ascii_case(".rgignore") {
            self.rgignore = true;
        }
    }

    fn contains(&self, name: &str) -> bool {
        match name {
            ".gitignore" => self.gitignore,
            ".ignore" => self.ignore,
            ".rgignore" => self.rgignore,
            _ => false,
        }
    }
}

enum WorkItem {
    Directory(DirectoryTask),
    Files(Vec<WalkEntry>),
    Error(WalkError),
}

fn expand_directory(
    walker: &NativeWalker,
    runtime: &WalkRuntime,
    task: DirectoryTask,
) -> Vec<WorkItem> {
    let mut output = Vec::new();
    expand_directory_streaming(
        walker,
        runtime,
        task,
        |item| {
            output.push(item);
            true
        },
        || true,
    );
    output
}

fn expand_directory_streaming<E, C>(
    walker: &NativeWalker,
    runtime: &WalkRuntime,
    task: DirectoryTask,
    mut emit: E,
    mut should_continue: C,
) where
    E: FnMut(WorkItem) -> bool,
    C: FnMut() -> bool,
{
    let mut entries = match fs::read_dir(&task.path) {
        Ok(entries) => entries,
        Err(error) => {
            emit(WorkItem::Error(WalkError::io(
                Some(task.display_path),
                error,
            )));
            return;
        }
    };

    let mut buffered = Vec::new();
    let mut present = IgnoreFilePresence::default();
    let mut presence_overflowed = false;
    for entry in entries.by_ref() {
        if !should_continue() {
            return;
        }
        if let Ok(entry) = &entry {
            present.observe(&entry.file_name());
        }
        buffered.push(entry);
        if buffered.len() > MAX_BUFFERED_DIRECTORY_ENTRIES {
            presence_overflowed = true;
            break;
        }
    }

    let known_presence = (!presence_overflowed).then_some(&present);
    let (context, context_errors) =
        walker.extend_context_for_directory(task.context, &task.path, known_presence);
    for error in context_errors {
        if !emit(WorkItem::Error(error)) {
            return;
        }
    }

    let mut files = Vec::with_capacity(FILE_BATCH_SIZE);
    for entry in buffered.into_iter().chain(entries) {
        if !should_continue() {
            return;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if !emit(WorkItem::Error(WalkError::io(
                    Some(task.display_path.clone()),
                    error,
                ))) {
                    return;
                }
                continue;
            }
        };
        let path = entry.path();
        let display_path = task.display_path.join(entry.file_name());
        match classify_entry(
            walker,
            runtime,
            &context,
            &task.root_base,
            entry,
            path,
            display_path,
        ) {
            ClassifiedEntry::Skip => {}
            ClassifiedEntry::File(file) => {
                files.push(file);
                if files.len() == FILE_BATCH_SIZE
                    && !emit(WorkItem::Files(std::mem::replace(
                        &mut files,
                        Vec::with_capacity(FILE_BATCH_SIZE),
                    )))
                {
                    return;
                }
            }
            ClassifiedEntry::Directory(directory) => {
                if !emit(WorkItem::Directory(directory)) {
                    return;
                }
            }
            ClassifiedEntry::Error(error) => {
                if !emit(WorkItem::Error(error)) {
                    return;
                }
            }
        }
    }
    if !files.is_empty() {
        emit(WorkItem::Files(files));
    }
}

enum ClassifiedEntry {
    Skip,
    File(WalkEntry),
    Directory(DirectoryTask),
    Error(WalkError),
}

fn classify_entry(
    walker: &NativeWalker,
    runtime: &WalkRuntime,
    context: &RuleChain,
    root_base: &Path,
    entry: DirEntry,
    path: PathBuf,
    display_path: PathBuf,
) -> ClassifiedEntry {
    let file_type = match entry.file_type() {
        Ok(file_type) => file_type,
        Err(error) => return ClassifiedEntry::Error(WalkError::io(Some(display_path), error)),
    };
    if file_type.is_symlink() {
        if !walker.options.follow_links {
            return ClassifiedEntry::Skip;
        }
        let target_metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => return ClassifiedEntry::Error(WalkError::io(Some(display_path), error)),
        };
        if target_metadata.is_file() {
            if should_emit_file(walker, context, root_base, &path) {
                return ClassifiedEntry::File(WalkEntry::new(
                    display_path,
                    path,
                    target_metadata.len(),
                ));
            }
            return ClassifiedEntry::Skip;
        }
        if target_metadata.is_dir() {
            return classify_directory(
                walker,
                runtime,
                context,
                root_base,
                path,
                display_path,
                true,
            );
        }
        return ClassifiedEntry::Skip;
    }
    if file_type.is_file() {
        if should_emit_file(walker, context, root_base, &path) {
            // Recursive walks avoid a separate metadata syscall for every
            // regular file. A zero length only disables the optional
            // sequential-read hint; it does not affect matching semantics.
            ClassifiedEntry::File(WalkEntry::new(display_path, path, 0))
        } else {
            ClassifiedEntry::Skip
        }
    } else {
        classify_directory(
            walker,
            runtime,
            context,
            root_base,
            path,
            display_path,
            file_type.is_dir(),
        )
    }
}

fn classify_directory(
    walker: &NativeWalker,
    runtime: &WalkRuntime,
    context: &RuleChain,
    root_base: &Path,
    path: PathBuf,
    display_path: PathBuf,
    is_directory: bool,
) -> ClassifiedEntry {
    if !is_directory || !should_descend(walker, context, root_base, &path) {
        return ClassifiedEntry::Skip;
    }
    match register_directory(runtime, &path, &display_path) {
        Ok(true) => ClassifiedEntry::Directory(DirectoryTask {
            path,
            display_path,
            context: context.clone(),
            root_base: root_base.to_path_buf(),
        }),
        Ok(false) => ClassifiedEntry::Skip,
        Err(error) => ClassifiedEntry::Error(error),
    }
}

fn should_emit_file(
    walker: &NativeWalker,
    context: &RuleChain,
    root_base: &Path,
    path: &Path,
) -> bool {
    match walker
        .user_globs
        .decision(path, &walker.current_dir, root_base)
    {
        GlobDecision::Exclude => return false,
        GlobDecision::Include => return true,
        GlobDecision::Unmatched if walker.user_globs.has_include => return false,
        GlobDecision::Unmatched => {}
    }
    if !walker.options.hidden && is_hidden_path(path) {
        return false;
    }
    !context.is_ignored(path, false)
}

fn should_descend(
    walker: &NativeWalker,
    context: &RuleChain,
    root_base: &Path,
    path: &Path,
) -> bool {
    match walker
        .user_globs
        .decision(path, &walker.current_dir, root_base)
    {
        GlobDecision::Exclude => return false,
        // A positive glob overrides both hidden and ignore filtering.
        GlobDecision::Include => return true,
        // An unmatched directory may still contain a positive match. Keep it
        // traversable even when it is hidden or ignored.
        GlobDecision::Unmatched if walker.user_globs.has_include => return true,
        GlobDecision::Unmatched => {}
    }
    if !walker.options.hidden && is_hidden_path(path) {
        return false;
    }
    !context.is_ignored(path, true) || context.may_include_descendant()
}

fn is_hidden_path(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with('.'))
}

fn register_directory(
    runtime: &WalkRuntime,
    path: &Path,
    display_path: &Path,
) -> Result<bool, WalkError> {
    let Some(visited) = &runtime.visited_directories else {
        return Ok(true);
    };
    let identity = fs::canonicalize(path)
        .map(normalize_directory_identity)
        .map_err(|error| WalkError::io(Some(display_path.to_path_buf()), error))?;
    let mut visited = visited.lock().unwrap_or_else(|error| error.into_inner());
    Ok(visited.insert(identity))
}

#[cfg(windows)]
fn normalize_directory_identity(path: PathBuf) -> PathBuf {
    // Windows paths are case-insensitive in the normal filesystem namespace.
    // Canonicalization resolves links; normalizing the resulting spelling makes
    // an alias with different casing share the same cycle-prevention key.
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(not(windows))]
fn normalize_directory_identity(path: PathBuf) -> PathBuf {
    path
}

struct WorkQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
    stopped: AtomicBool,
}

struct QueueState {
    ready: VecDeque<WorkItem>,
    directories: VecDeque<WorkItem>,
    active: usize,
}

impl WorkQueue {
    fn new(items: Vec<WorkItem>) -> Self {
        let mut ready = VecDeque::new();
        let mut directories = VecDeque::new();
        for item in items {
            match item {
                WorkItem::Directory(_) => directories.push_back(item),
                WorkItem::Files(_) | WorkItem::Error(_) => ready.push_back(item),
            }
        }
        Self {
            state: Mutex::new(QueueState {
                ready,
                directories,
                active: 0,
            }),
            ready: Condvar::new(),
            stopped: AtomicBool::new(false),
        }
    }

    fn take(&self) -> Option<WorkLease<'_>> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if self.is_stopped() {
                return None;
            }
            if let Some(item) = state
                .ready
                .pop_front()
                .or_else(|| state.directories.pop_front())
            {
                state.active += 1;
                return Some(WorkLease {
                    queue: self,
                    item: Some(item),
                    completed: false,
                });
            }
            if state.active == 0 {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    fn publish(&self, item: WorkItem) -> bool {
        if self.is_stopped() {
            return false;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if self.is_stopped() {
            return false;
        }
        match item {
            WorkItem::Directory(_) => state.directories.push_back(item),
            WorkItem::Files(_) | WorkItem::Error(_) => state.ready.push_back(item),
        }
        self.ready.notify_one();
        true
    }

    fn complete(&self, quit: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        debug_assert!(state.active > 0);
        state.active = state.active.saturating_sub(1);
        if quit {
            self.stopped.store(true, Ordering::Release);
            state.ready.clear();
            state.directories.clear();
        }
        self.ready.notify_all();
    }

    fn abandon(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        debug_assert!(state.active > 0);
        state.active = state.active.saturating_sub(1);
        self.stopped.store(true, Ordering::Release);
        state.ready.clear();
        state.directories.clear();
        self.ready.notify_all();
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

struct WorkLease<'a> {
    queue: &'a WorkQueue,
    item: Option<WorkItem>,
    completed: bool,
}

impl WorkLease<'_> {
    fn take_item(&mut self) -> WorkItem {
        self.item.take().expect("work lease always owns one item")
    }

    fn finish(mut self, quit: bool) {
        self.queue.complete(quit);
        self.completed = true;
    }
}

impl Drop for WorkLease<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.queue.abandon();
        }
    }
}

fn worker_loop<V>(walker: &NativeWalker, runtime: &WalkRuntime, queue: &WorkQueue, visitor: &mut V)
where
    V: FnMut(Result<WalkEntry, WalkError>) -> WalkControl,
{
    while let Some(mut lease) = queue.take() {
        let item = lease.take_item();
        let quit = match item {
            WorkItem::Error(error) => visitor(Err(error)) == WalkControl::Quit,
            WorkItem::Files(files) => {
                let mut quit = false;
                for file in files {
                    if visitor(Ok(file)) == WalkControl::Quit {
                        quit = true;
                        break;
                    }
                }
                quit
            }
            WorkItem::Directory(task) => {
                expand_directory_streaming(
                    walker,
                    runtime,
                    task,
                    |item| queue.publish(item),
                    || !queue.is_stopped(),
                );
                false
            }
        };
        lease.finish(quit);
        if quit {
            return;
        }
    }
}

#[derive(Clone)]
struct RuleChain {
    node: Option<Arc<RuleNode>>,
    explicit_root: Option<Arc<PathBuf>>,
}

struct RuleNode {
    parent: RuleChain,
    layer: IgnoreLayer,
}

impl RuleChain {
    fn empty() -> Self {
        Self {
            node: None,
            explicit_root: None,
        }
    }

    fn with_explicit_root(mut self, root: PathBuf) -> Self {
        self.explicit_root = Some(Arc::new(root));
        self
    }

    fn extend(&self, layer: IgnoreLayer) -> Self {
        Self {
            node: Some(Arc::new(RuleNode {
                parent: self.clone(),
                layer,
            })),
            explicit_root: self.explicit_root.clone(),
        }
    }

    fn is_ignored(&self, path: &Path, is_directory: bool) -> bool {
        self.decision(path, is_directory) == IgnoreDecision::Ignore
    }

    fn decision(&self, path: &Path, is_directory: bool) -> IgnoreDecision {
        self.decision_with_root(
            path,
            is_directory,
            self.explicit_root.as_ref().map(|root| root.as_path()),
        )
    }

    fn decision_with_root(
        &self,
        path: &Path,
        is_directory: bool,
        explicit_root: Option<&Path>,
    ) -> IgnoreDecision {
        let Some(node) = &self.node else {
            return IgnoreDecision::NoMatch;
        };
        let mut decision = node
            .parent
            .decision_with_root(path, is_directory, explicit_root);
        if let Some(next) = node.layer.decision(path, is_directory, explicit_root) {
            decision = next;
        }
        decision
    }

    fn may_include_descendant(&self) -> bool {
        self.node.as_ref().is_some_and(|node| {
            node.layer.has_include_descendant || node.parent.may_include_descendant()
        })
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum IgnoreDecision {
    NoMatch,
    Ignore,
    Include,
}

#[derive(Clone)]
struct IgnoreLayer {
    base: PathBuf,
    rules: Arc<[IgnoreRule]>,
    has_include_descendant: bool,
}

impl IgnoreLayer {
    fn from_templates(base: PathBuf, templates: Arc<[RuleTemplate]>) -> Self {
        let rules = templates
            .iter()
            .cloned()
            .map(IgnoreRule::from_template)
            .collect::<Vec<_>>();
        let has_include_descendant = rules.iter().any(|rule| rule.include);
        Self {
            base,
            rules: rules.into(),
            has_include_descendant,
        }
    }

    fn decision(
        &self,
        path: &Path,
        is_directory: bool,
        explicit_root: Option<&Path>,
    ) -> Option<IgnoreDecision> {
        if self.rules.is_empty() || !path.starts_with(&self.base) {
            return None;
        }
        let mut decision = None;
        for rule in self.rules.iter() {
            if self.rule_matches(rule, path, is_directory, explicit_root) {
                decision = Some(if rule.include {
                    IgnoreDecision::Include
                } else {
                    IgnoreDecision::Ignore
                });
            }
        }
        decision
    }

    fn rule_matches(
        &self,
        rule: &IgnoreRule,
        path: &Path,
        is_directory: bool,
        explicit_root: Option<&Path>,
    ) -> bool {
        if (!rule.directory_only || is_directory)
            && rule_matches_path(&rule.pattern, &self.base, path)
        {
            return true;
        }

        // A rule that selects a directory (with or without a final slash)
        // ignores its subtree. Test ancestors so a later `!build/keep.txt`
        // can re-include only the intended file while siblings stay ignored.
        let mut candidate = path.parent();
        while let Some(directory) = candidate {
            if !directory.starts_with(&self.base) {
                break;
            }
            if explicit_root.is_some_and(|root| directory == root) {
                break;
            }
            if rule_matches_path(&rule.pattern, &self.base, directory) {
                return true;
            }
            if directory == self.base {
                break;
            }
            candidate = directory.parent();
        }
        false
    }
}

fn rule_matches_path(pattern: &CompiledGlob, base: &Path, path: &Path) -> bool {
    if pattern.path_pattern {
        relative_slash_path(base, path).is_some_and(|text| pattern.matches(&text))
    } else {
        file_name_text(path).is_some_and(|name| pattern.matches(&name))
    }
}

#[derive(Clone)]
struct IgnoreRule {
    pattern: CompiledGlob,
    include: bool,
    directory_only: bool,
}

impl IgnoreRule {
    fn from_template(template: RuleTemplate) -> Self {
        Self {
            pattern: template.pattern,
            include: template.include,
            directory_only: template.directory_only,
        }
    }
}

#[derive(Clone)]
struct RuleTemplate {
    pattern: CompiledGlob,
    include: bool,
    directory_only: bool,
}

fn load_ignore_layer(path: &Path, base: &Path) -> Result<Option<IgnoreLayer>, WalkError> {
    let bytes = match read_bounded_ignore_file(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(error) => return Err(WalkError::io(Some(path.to_path_buf()), error)),
    };
    let text = String::from_utf8_lossy(&bytes);
    let templates = parse_ignore_templates(&text);
    if templates.is_empty() {
        return Ok(None);
    }
    Ok(Some(IgnoreLayer::from_templates(
        base.to_path_buf(),
        templates.into(),
    )))
}

fn read_bounded_ignore_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take((MAX_IGNORE_FILE_BYTES as u64) + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_IGNORE_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ignore file exceeds {MAX_IGNORE_FILE_BYTES} bytes"),
        ));
    }
    Ok(Some(bytes))
}

fn parse_ignore_templates(text: &str) -> Vec<RuleTemplate> {
    text.lines()
        .filter_map(parse_ignore_line)
        .collect::<Vec<_>>()
}

fn parse_ignore_line(line: &str) -> Option<RuleTemplate> {
    let mut line = line.strip_suffix('\r').unwrap_or(line);
    if line.starts_with('\u{feff}') {
        line = &line['\u{feff}'.len_utf8()..];
    }
    line = trim_unescaped_trailing_spaces(line);
    if line.is_empty() {
        return None;
    }
    if line.starts_with('#') {
        return None;
    }

    let (include, line) = if let Some(rest) = line.strip_prefix('!') {
        (true, rest)
    } else {
        (false, line)
    };
    if line.is_empty() {
        return None;
    }
    let (directory_only, line) = strip_unescaped_suffix(line, '/');
    if line.is_empty() {
        return None;
    }
    let anchored = line.starts_with('/');
    let line = line.strip_prefix('/').unwrap_or(line);
    if line.is_empty() {
        return None;
    }
    if line.len() > MAX_USER_GLOB_BYTES
        || (line.len() > MAX_GENERAL_GLOB_BYTES && glob_requires_general_engine(line))
    {
        return None;
    }
    Some(RuleTemplate {
        pattern: CompiledGlob::for_ignore(line, anchored),
        include,
        directory_only,
    })
}

fn trim_unescaped_trailing_spaces(mut value: &str) -> &str {
    while value.ends_with(' ') && !is_escaped_at(value, value.len() - 1) {
        value = &value[..value.len() - 1];
    }
    value
}

fn strip_unescaped_suffix(value: &str, suffix: char) -> (bool, &str) {
    let Some((index, last)) = value.char_indices().last() else {
        return (false, value);
    };
    if last != suffix || is_escaped_at(value, index) {
        return (false, value);
    }
    (true, &value[..index])
}

fn is_escaped_at(value: &str, index: usize) -> bool {
    let bytes = value.as_bytes();
    let mut slash_count = 0;
    let mut cursor = index;
    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        slash_count += 1;
        cursor -= 1;
    }
    slash_count % 2 == 1
}

fn load_global_rule_templates() -> Vec<RuleTemplate> {
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("GIT_CONFIG_GLOBAL")
        && !path.is_empty()
    {
        candidates.push(PathBuf::from(path));
    }
    if let Some(home) = user_home() {
        candidates.push(home.join(".gitconfig"));
        if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
            candidates.push(PathBuf::from(config_home).join("git").join("config"));
        } else {
            candidates.push(home.join(".config").join("git").join("config"));
        }
    }

    let mut exclude_files = Vec::new();
    for config in candidates {
        if let Some(path) = parse_global_exclude_file(&config) {
            exclude_files.push(path);
        }
    }
    if exclude_files.is_empty() {
        if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
            exclude_files.push(PathBuf::from(config_home).join("git").join("ignore"));
        } else if let Some(home) = user_home() {
            exclude_files.push(home.join(".config").join("git").join("ignore"));
        }
    }

    let mut rules = Vec::new();
    for path in exclude_files {
        let Ok(Some(bytes)) = read_bounded_ignore_file(&path) else {
            continue;
        };
        rules.extend(parse_ignore_templates(&String::from_utf8_lossy(&bytes)));
    }
    rules
}

/// Return the Git global excludes file used by this process, when it can be
/// determined without launching Git. The cache module uses this to invalidate
/// a collected path list when the user's global rules change.
pub(crate) fn global_git_ignore_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("GIT_CONFIG_GLOBAL")
        && !path.is_empty()
    {
        candidates.push(PathBuf::from(path));
    }
    if let Some(home) = user_home() {
        candidates.push(home.join(".gitconfig"));
        if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
            candidates.push(PathBuf::from(config_home).join("git").join("config"));
        } else {
            candidates.push(home.join(".config").join("git").join("config"));
        }
    }
    for config in candidates {
        if let Some(path) = parse_global_exclude_file(&config) {
            return Some(path);
        }
    }
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(config_home).join("git").join("ignore"));
    }
    user_home().map(|home| home.join(".config").join("git").join("ignore"))
}

fn parse_global_exclude_file(config_path: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(config_path).ok()?;
    let mut in_core = false;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let section = line[1..line.len() - 1]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            in_core = section.eq_ignore_ascii_case("core");
            continue;
        }
        if !in_core {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("excludesfile") {
            continue;
        }
        let value = value.trim().trim_matches('"');
        if value.is_empty() {
            continue;
        }
        return Some(expand_git_path(value, config_path.parent()));
    }
    None
}

fn expand_git_path(value: &str, config_parent: Option<&Path>) -> PathBuf {
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
        && let Some(home) = user_home()
    {
        return home.join(rest);
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        config_parent.unwrap_or_else(|| Path::new(".")).join(path)
    }
}

fn user_home() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
}

fn git_info_exclude_paths(directory: &Path) -> Vec<PathBuf> {
    let dot_git = directory.join(".git");
    let Ok(metadata) = fs::symlink_metadata(&dot_git) else {
        return Vec::new();
    };
    if metadata.is_dir() {
        return vec![dot_git.join("info").join("exclude")];
    }
    if !metadata.is_file() {
        return Vec::new();
    }
    let Ok(text) = fs::read_to_string(&dot_git) else {
        return Vec::new();
    };
    let target = text.lines().find_map(|line| line.strip_prefix("gitdir:"));
    let Some(target) = target else {
        return Vec::new();
    };
    let target = target.trim();
    if target.is_empty() {
        return Vec::new();
    }
    let target = PathBuf::from(target);
    let git_dir = if target.is_absolute() {
        target
    } else {
        directory.join(target)
    };
    let mut excludes = Vec::with_capacity(2);
    // A linked worktree normally reads the shared common directory's exclude
    // first, then lets a worktree-specific gitdir exclude override it.
    if let Some(common_dir) = git_common_dir(&git_dir) {
        let common_exclude = common_dir.join("info").join("exclude");
        if common_exclude != git_dir.join("info").join("exclude") {
            excludes.push(common_exclude);
        }
    }
    excludes.push(git_dir.join("info").join("exclude"));
    excludes
}

fn git_common_dir(git_dir: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(git_dir.join("commondir")).ok()?;
    let value = contents.lines().next()?.trim();
    if value.is_empty() {
        return None;
    }
    let common_dir = PathBuf::from(value);
    Some(if common_dir.is_absolute() {
        common_dir
    } else {
        // Git specifies a linked-worktree `commondir` relative to the gitdir.
        git_dir.join(common_dir)
    })
}

#[derive(Clone)]
struct UserGlobSet {
    rules: Arc<[UserGlobRule]>,
    has_include: bool,
}

impl UserGlobSet {
    fn parse(values: &[String]) -> Result<Self, GlobError> {
        let mut rules = Vec::with_capacity(values.len());
        let mut has_include = false;
        for raw in values {
            let (include, patterns) = parse_user_glob(raw)?;
            has_include |= include;
            rules.extend(
                patterns
                    .into_iter()
                    .map(|pattern| UserGlobRule { pattern, include }),
            );
        }
        Ok(Self {
            rules: rules.into(),
            has_include,
        })
    }

    fn decision(&self, path: &Path, current_dir: &Path, root_base: &Path) -> GlobDecision {
        if self.rules.is_empty() {
            return GlobDecision::Unmatched;
        }
        let current_relative = relative_slash_path(current_dir, path);
        let root_relative = relative_slash_path(root_base, path);
        let basename = file_name_text(path);
        let mut decision = GlobDecision::Unmatched;
        for rule in self.rules.iter() {
            let candidate = if rule.pattern.path_pattern {
                current_relative
                    .as_deref()
                    .or(root_relative.as_deref())
                    .unwrap_or_default()
            } else {
                basename.as_deref().unwrap_or_default()
            };
            if rule.pattern.matches(candidate) {
                decision = if rule.include {
                    GlobDecision::Include
                } else {
                    GlobDecision::Exclude
                };
            }
        }
        decision
    }
}

#[derive(Clone)]
struct UserGlobRule {
    pattern: CompiledGlob,
    include: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum GlobDecision {
    Unmatched,
    Include,
    Exclude,
}

fn parse_user_glob(raw: &str) -> Result<(bool, Vec<CompiledGlob>), GlobError> {
    if raw.is_empty() {
        return Err(GlobError::new(
            Some(raw.to_owned()),
            "空の glob は指定できません",
        ));
    }
    if raw.len() > MAX_USER_GLOB_BYTES {
        return Err(GlobError::new(
            Some(raw.to_owned()),
            format!("glob は {MAX_USER_GLOB_BYTES} bytes 以下で指定してください"),
        ));
    }
    let (include, raw_pattern) = if let Some(pattern) = raw.strip_prefix('!') {
        (false, pattern)
    } else {
        (true, raw)
    };
    if raw_pattern.is_empty() {
        return Err(GlobError::new(
            Some(raw.to_owned()),
            "glob のパターンが空です",
        ));
    }
    validate_user_glob(raw_pattern, raw)?;
    let patterns = expand_braces(raw_pattern, raw)?
        .into_iter()
        .map(|pattern| CompiledGlob::for_user(&pattern))
        .collect();
    Ok((include, patterns))
}

fn validate_user_glob(pattern: &str, original: &str) -> Result<(), GlobError> {
    if pattern.as_bytes().contains(&0) {
        return Err(GlobError::new(
            Some(original.to_owned()),
            "glob に NUL 文字は使えません",
        ));
    }
    if pattern.len() > MAX_GENERAL_GLOB_BYTES && glob_requires_general_engine(pattern) {
        return Err(GlobError::new(
            Some(original.to_owned()),
            format!("複雑なglobは {MAX_GENERAL_GLOB_BYTES} bytes 以下で指定してください"),
        ));
    }
    let bytes = pattern.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'[' || is_escaped_at(pattern, index) {
            index += 1;
            continue;
        }
        let mut close = index + 1;
        while close < bytes.len() && (bytes[close] != b']' || is_escaped_at(pattern, close)) {
            close += 1;
        }
        if close == bytes.len() {
            return Err(GlobError::new(
                Some(original.to_owned()),
                "glob の文字クラス `[` が閉じられていません",
            ));
        }
        index = close + 1;
    }
    Ok(())
}

fn glob_requires_general_engine(pattern: &str) -> bool {
    let mut saw_star = false;
    for byte in pattern.bytes() {
        match byte {
            b'*' if saw_star => return true,
            b'*' => saw_star = true,
            b'?' | b'[' | b'\\' => return true,
            _ => {}
        }
    }
    false
}

fn expand_braces(pattern: &str, original: &str) -> Result<Vec<String>, GlobError> {
    let mut pending = vec![pattern.to_owned()];
    let mut expanded = Vec::new();
    let mut generated = 1_usize;
    while let Some(candidate) = pending.pop() {
        let Some((open, close)) = find_expandable_brace(&candidate, original)? else {
            expanded.push(candidate);
            continue;
        };
        let parts = split_brace_parts(&candidate[open + 1..close]);
        generated = generated.saturating_add(parts.len().saturating_sub(1));
        if generated > MAX_BRACE_EXPANSIONS {
            return Err(GlobError::new(
                Some(original.to_owned()),
                format!("glob の brace 展開は {MAX_BRACE_EXPANSIONS} 個までです"),
            ));
        }
        for part in parts.into_iter().rev() {
            let mut value =
                String::with_capacity(candidate.len() - (close - open + 1) + part.len());
            value.push_str(&candidate[..open]);
            value.push_str(part);
            value.push_str(&candidate[close + 1..]);
            pending.push(value);
        }
    }
    Ok(expanded)
}

fn find_expandable_brace(
    pattern: &str,
    original: &str,
) -> Result<Option<(usize, usize)>, GlobError> {
    let bytes = pattern.as_bytes();
    let mut openings = Vec::new();
    let mut in_class = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if is_escaped_at(pattern, index) {
            continue;
        }
        match byte {
            b'[' => in_class = true,
            b']' => in_class = false,
            b'{' if !in_class => openings.push(index),
            b'}' if !in_class => {
                let Some(open) = openings.pop() else {
                    return Err(GlobError::new(
                        Some(original.to_owned()),
                        "glob の `}` に対応する `{` がありません",
                    ));
                };
                if contains_unescaped_comma(&pattern[open + 1..index]) {
                    return Ok(Some((open, index)));
                }
            }
            _ => {}
        }
    }
    if !openings.is_empty() {
        return Err(GlobError::new(
            Some(original.to_owned()),
            "glob の `{` が閉じられていません",
        ));
    }
    Ok(None)
}

fn contains_unescaped_comma(value: &str) -> bool {
    value
        .bytes()
        .enumerate()
        .any(|(index, byte)| byte == b',' && !is_escaped_at(value, index))
}

fn split_brace_parts(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (index, byte) in value.bytes().enumerate() {
        if byte == b',' && !is_escaped_at(value, index) {
            parts.push(&value[start..index]);
            start = index + 1;
        }
    }
    parts.push(&value[start..]);
    parts
}

#[derive(Clone)]
struct CompiledGlob {
    pattern: String,
    path_pattern: bool,
    kind: GlobKind,
}

#[derive(Clone)]
enum GlobKind {
    Literal,
    OneStar { index: usize },
    General,
}

impl CompiledGlob {
    fn for_ignore(pattern: &str, anchored: bool) -> Self {
        let path_pattern = anchored || pattern.contains('/');
        Self::new(pattern, path_pattern)
    }

    fn for_user(pattern: &str) -> Self {
        let anchored = pattern.starts_with('/');
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        let path_pattern = anchored || pattern.contains('/');
        Self::new(pattern, path_pattern)
    }

    fn new(pattern: &str, path_pattern: bool) -> Self {
        let mut star = None;
        let mut general = false;
        for (index, byte) in pattern.bytes().enumerate() {
            match byte {
                b'*' if star.replace(index).is_some() => general = true,
                b'?' | b'[' | b'\\' => general = true,
                _ => {}
            }
        }
        let kind = if general {
            GlobKind::General
        } else if let Some(index) = star {
            GlobKind::OneStar { index }
        } else {
            GlobKind::Literal
        };
        Self {
            pattern: pattern.to_owned(),
            path_pattern,
            kind,
        }
    }

    fn matches(&self, text: &str) -> bool {
        match self.kind {
            GlobKind::Literal => folded_bytes_equal(self.pattern.as_bytes(), text.as_bytes()),
            GlobKind::OneStar { index } => {
                let pattern = self.pattern.as_bytes();
                let prefix = &pattern[..index];
                let suffix = &pattern[index + 1..];
                text.len() >= prefix.len() + suffix.len()
                    && folded_bytes_equal(prefix, &text.as_bytes()[..prefix.len()])
                    && folded_bytes_equal(suffix, &text.as_bytes()[text.len() - suffix.len()..])
                    && !text.as_bytes()[prefix.len()..text.len() - suffix.len()].contains(&b'/')
            }
            GlobKind::General => glob_matches(&self.pattern, text),
        }
    }
}

fn folded_bytes_equal(expected: &[u8], actual: &[u8]) -> bool {
    expected.len() == actual.len()
        && expected.iter().zip(actual).all(|(&expected, &actual)| {
            #[cfg(windows)]
            let equal = expected.eq_ignore_ascii_case(&actual);
            #[cfg(not(windows))]
            let equal = expected == actual;
            equal
        })
}

/// Small matcher for the glob subset used by Git ignores and FastFs `-g`:
/// `*`, `?`, `**`, character classes and backslash escaping. Common literal
/// and one-star patterns take allocation-free fast paths in `CompiledGlob`.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let mut memo = HashMap::new();
    glob_matches_from(pattern, text, 0, 0, &mut memo)
}

/// Cache every `(pattern byte, text byte)` state. Wildcard alternatives then
/// share their suffix work instead of forming the exponential retry tree that
/// patterns such as `*a*a*a*` otherwise create.
fn glob_matches_from(
    pattern: &[u8],
    text: &[u8],
    pattern_index: usize,
    text_index: usize,
    memo: &mut HashMap<(usize, usize), bool>,
) -> bool {
    if let Some(result) = memo.get(&(pattern_index, text_index)) {
        return *result;
    }
    if memo.len() >= MAX_GLOB_MEMO_STATES {
        return false;
    }
    let result = if pattern_index == pattern.len() {
        text_index == text.len()
    } else {
        match pattern[pattern_index] {
            b'*' => {
                let double_star = pattern.get(pattern_index + 1) == Some(&b'*');
                let after_star = pattern_index + if double_star { 2 } else { 1 };
                if double_star && pattern.get(after_star) == Some(&b'/') {
                    let next_pattern = after_star + 1;
                    if glob_matches_from(pattern, text, next_pattern, text_index, memo) {
                        true
                    } else {
                        let mut cursor = text_index;
                        let mut matched = false;
                        while cursor < text.len() {
                            if text[cursor] == b'/'
                                && glob_matches_from(pattern, text, next_pattern, cursor + 1, memo)
                            {
                                matched = true;
                                break;
                            }
                            cursor = next_utf8_boundary(text, cursor);
                        }
                        matched
                    }
                } else {
                    let mut cursor = text_index;
                    let mut matched = false;
                    loop {
                        if glob_matches_from(pattern, text, after_star, cursor, memo) {
                            matched = true;
                            break;
                        }
                        if cursor == text.len() || (!double_star && text[cursor] == b'/') {
                            break;
                        }
                        cursor = next_utf8_boundary(text, cursor);
                    }
                    matched
                }
            }
            b'?' if text_index < text.len() && text[text_index] != b'/' => glob_matches_from(
                pattern,
                text,
                pattern_index + 1,
                next_utf8_boundary(text, text_index),
                memo,
            ),
            b'[' => character_class_matches(pattern, pattern_index, text, text_index).is_some_and(
                |(next_pattern, next_text)| {
                    glob_matches_from(pattern, text, next_pattern, next_text, memo)
                },
            ),
            b'\\' if pattern_index + 1 < pattern.len() => {
                literal_char_matches(pattern, pattern_index + 1, text, text_index).is_some_and(
                    |(next_pattern, next_text)| {
                        glob_matches_from(pattern, text, next_pattern, next_text, memo)
                    },
                )
            }
            _ => literal_char_matches(pattern, pattern_index, text, text_index).is_some_and(
                |(next_pattern, next_text)| {
                    glob_matches_from(pattern, text, next_pattern, next_text, memo)
                },
            ),
        }
    };
    memo.insert((pattern_index, text_index), result);
    result
}

fn literal_char_matches(
    pattern: &[u8],
    pattern_index: usize,
    text: &[u8],
    text_index: usize,
) -> Option<(usize, usize)> {
    if pattern_index >= pattern.len() || text_index >= text.len() {
        return None;
    }
    let pattern_end = next_utf8_boundary(pattern, pattern_index);
    let text_end = next_utf8_boundary(text, text_index);
    if pattern_end - pattern_index != text_end - text_index {
        return None;
    }
    for offset in 0..(pattern_end - pattern_index) {
        let expected = pattern[pattern_index + offset];
        let actual = text[text_index + offset];
        #[cfg(windows)]
        let equal = expected.eq_ignore_ascii_case(&actual);
        #[cfg(not(windows))]
        let equal = expected == actual;
        if !equal {
            return None;
        }
    }
    Some((pattern_end, text_end))
}

fn character_class_matches(
    pattern: &[u8],
    start: usize,
    text: &[u8],
    text_index: usize,
) -> Option<(usize, usize)> {
    if text_index == text.len() || text[text_index] == b'/' {
        return None;
    }
    let mut cursor = start + 1;
    let negated = matches!(pattern.get(cursor), Some(b'!') | Some(b'^'));
    if negated {
        cursor += 1;
    }
    let class_start = cursor;
    if pattern.get(cursor) == Some(&b']') {
        cursor += 1;
    }
    while cursor < pattern.len() && pattern[cursor] != b']' {
        cursor += 1;
    }
    if cursor == pattern.len() || cursor == class_start {
        // An unmatched `[` is a literal for ignore files. User globs are
        // validated before reaching here.
        return literal_char_matches(pattern, start, text, text_index);
    }
    let actual = fold_ascii(text[text_index]);
    let mut matched = false;
    let mut item = class_start;
    while item < cursor {
        let first = pattern[item];
        if item + 2 < cursor && pattern[item + 1] == b'-' {
            let last = pattern[item + 2];
            let low = fold_ascii(first);
            let high = fold_ascii(last);
            if low <= actual && actual <= high {
                matched = true;
            }
            item += 3;
        } else {
            if fold_ascii(first) == actual {
                matched = true;
            }
            item += 1;
        }
    }
    (matched != negated).then_some((cursor + 1, next_utf8_boundary(text, text_index)))
}

fn fold_ascii(value: u8) -> u8 {
    #[cfg(windows)]
    {
        value.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        value
    }
}

fn next_utf8_boundary(bytes: &[u8], index: usize) -> usize {
    let Some(byte) = bytes.get(index).copied() else {
        return index;
    };
    if byte < 0x80 {
        index + 1
    } else if byte & 0b1110_0000 == 0b1100_0000 {
        (index + 2).min(bytes.len())
    } else if byte & 0b1111_0000 == 0b1110_0000 {
        (index + 3).min(bytes.len())
    } else if byte & 0b1111_1000 == 0b1111_0000 {
        (index + 4).min(bytes.len())
    } else {
        index + 1
    }
}

fn relative_slash_path(base: &Path, path: &Path) -> Option<String> {
    if let Ok(relative) = path.strip_prefix(base) {
        return relative_components_to_slash(relative.components());
    }

    let base_components = base.components().collect::<Vec<_>>();
    let path_components = path.components().collect::<Vec<_>>();
    let mut common = 0;
    while common < base_components.len()
        && common < path_components.len()
        && components_equal(base_components[common], path_components[common])
    {
        common += 1;
    }
    if common == 0
        || base_components[..common]
            .iter()
            .filter(|component| matches!(component, Component::RootDir | Component::Prefix(_)))
            .count()
            != path_components[..common]
                .iter()
                .filter(|component| matches!(component, Component::RootDir | Component::Prefix(_)))
                .count()
    {
        return None;
    }

    let mut text = String::new();
    for component in &base_components[common..] {
        if matches!(component, Component::Normal(_)) {
            push_slash_component(&mut text, OsStr::new(".."));
        } else if matches!(component, Component::RootDir | Component::Prefix(_)) {
            return None;
        }
    }
    for component in &path_components[common..] {
        let value = match component {
            Component::Normal(value) => value,
            Component::CurDir => continue,
            Component::ParentDir => OsStr::new(".."),
            Component::RootDir | Component::Prefix(_) => return None,
        };
        push_slash_component(&mut text, value);
    }
    Some(text)
}

fn relative_components_to_slash<'a>(
    components: impl Iterator<Item = Component<'a>>,
) -> Option<String> {
    let mut text = String::new();
    for component in components {
        let value = match component {
            Component::Normal(value) => value,
            Component::CurDir => continue,
            Component::ParentDir => OsStr::new(".."),
            Component::RootDir | Component::Prefix(_) => return None,
        };
        push_slash_component(&mut text, value);
    }
    Some(text)
}

fn components_equal(left: Component<'_>, right: Component<'_>) -> bool {
    match (left, right) {
        (Component::RootDir, Component::RootDir)
        | (Component::CurDir, Component::CurDir)
        | (Component::ParentDir, Component::ParentDir) => true,
        (Component::Prefix(left), Component::Prefix(right)) => {
            os_strings_equal(left.as_os_str(), right.as_os_str())
        }
        (Component::Normal(left), Component::Normal(right)) => os_strings_equal(left, right),
        _ => false,
    }
}

fn os_strings_equal(left: &OsStr, right: &OsStr) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn push_slash_component(destination: &mut String, value: &OsStr) {
    if !destination.is_empty() {
        destination.push('/');
    }
    push_lossy(destination, value);
}

fn file_name_text(path: &Path) -> Option<Cow<'_, str>> {
    path.file_name().map(OsStr::to_string_lossy)
}

fn push_lossy(destination: &mut String, value: &OsStr) {
    destination.push_str(&value.to_string_lossy());
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    absolute_path_from(&env::current_dir()?, path)
}

fn absolute_path_from(base: &Path, path: &Path) -> io::Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    Ok(lexical_normalize(&joined))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // `absolute_path_from` supplies an absolute base, so a parent
                // can never legitimately escape the filesystem root/prefix.
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::{NativeWalkOptions, NativeWalker, WalkControl, glob_matches, parse_user_glob};
    use std::fs;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::{Component, Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_directory(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("fastfs-native-walker-{label}-{now}-{nonce}"));
        fs::create_dir_all(&path).expect("create test directory");
        path
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create test parent");
        }
        fs::write(path, contents).expect("write test file");
    }

    fn walked_names(walker: &NativeWalker) -> Vec<String> {
        let mut names = walker
            .build()
            .map(|entry| {
                entry
                    .expect("walk success")
                    .path()
                    .file_name()
                    .expect("file name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn positive_glob_overrides_hidden_and_ignore_rules() {
        let root = temp_directory("glob-override");
        write(&root, ".ignore", "ignored.txt\n");
        write(&root, "plain.txt", "plain");
        write(&root, ".hidden.txt", "hidden");
        write(&root, "ignored.txt", "ignored");
        write(&root, "other.rs", "other");

        let default = NativeWalker::new(vec![root.clone()], NativeWalkOptions::default()).unwrap();
        assert_eq!(walked_names(&default), ["other.rs", "plain.txt"]);

        let with_glob = NativeWalker::new(
            vec![root.clone()],
            NativeWalkOptions {
                globs: vec!["*.txt".to_owned()],
                ..NativeWalkOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            walked_names(&with_glob),
            [".hidden.txt", "ignored.txt", "plain.txt"]
        );

        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn explicit_file_root_overrides_user_globs() {
        let root = temp_directory("explicit-file-glob");
        let file = root.join("direct.rs");
        write(&root, "direct.rs", "direct");

        let walker = NativeWalker::new(
            vec![file],
            NativeWalkOptions {
                globs: vec!["!*.rs".to_owned()],
                ..NativeWalkOptions::default()
            },
        )
        .unwrap();
        assert_eq!(walked_names(&walker), ["direct.rs"]);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn no_ignore_keeps_hidden_filter_but_reads_ignored_file() {
        let root = temp_directory("no-ignore");
        write(&root, ".gitignore", "priority.txt\n");
        write(&root, ".ignore", "!priority.txt\n");
        write(&root, ".rgignore", "priority.txt\n");
        write(&root, ".hidden.txt", "hidden");
        write(&root, "priority.txt", "ignored by highest-priority file");

        let default = NativeWalker::new(vec![root.clone()], NativeWalkOptions::default()).unwrap();
        assert!(walked_names(&default).is_empty());

        let walker = NativeWalker::new(
            vec![root.clone()],
            NativeWalkOptions {
                no_ignore: true,
                ..NativeWalkOptions::default()
            },
        )
        .unwrap();
        assert_eq!(walked_names(&walker), ["priority.txt"]);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn parent_ignore_and_negation_are_applied_in_order() {
        let root = temp_directory("parent-ignore");
        let child = root.join("child");
        write(&root, ".ignore", "*.log\n!keep.log\n");
        write(&root, "child/drop.log", "drop");
        write(&root, "child/keep.log", "keep");
        write(&root, "child/keep.txt", "keep");

        let walker = NativeWalker::new(vec![child.clone()], NativeWalkOptions::default()).unwrap();
        let mut names = walked_names(&walker);
        names.sort();
        assert_eq!(names, ["keep.log", "keep.txt"]);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn explicit_ignored_root_keeps_other_parent_file_rules() {
        let root = temp_directory("explicit-ignored-root");
        let build = root.join("build");
        write(&root, ".ignore", "build/\n*.log\n");
        write(&root, "build/drop.log", "drop");
        write(&root, "build/keep.txt", "keep");

        let walker = NativeWalker::new(vec![build], NativeWalkOptions::default()).unwrap();
        assert_eq!(walked_names(&walker), ["keep.txt"]);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn anchored_and_escaped_ignore_patterns_keep_their_git_meaning() {
        let root = temp_directory("anchored-ignore");
        write(&root, ".ignore", "/foo\n\\!literal.txt\n\\#hash.txt\n");
        write(&root, "foo", "root");
        write(&root, "nested/foo", "nested");
        write(&root, "!literal.txt", "literal");
        write(&root, "#hash.txt", "hash");
        write(&root, "visible.txt", "visible");

        let walker = NativeWalker::new(vec![root.clone()], NativeWalkOptions::default()).unwrap();
        assert_eq!(walked_names(&walker), ["foo", "visible.txt"]);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn directory_ignore_can_be_narrowly_reincluded() {
        let root = temp_directory("directory-negation");
        write(&root, ".ignore", "build/\n!build/keep.txt\n");
        write(&root, "build/drop.txt", "drop");
        write(&root, "build/keep.txt", "keep");

        let walker = NativeWalker::new(vec![root.clone()], NativeWalkOptions::default()).unwrap();
        assert_eq!(walked_names(&walker), ["keep.txt"]);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn ordinary_directory_match_also_applies_to_its_subtree() {
        let root = temp_directory("ordinary-directory-ignore");
        write(&root, ".ignore", "build\n!build/keep.txt\n");
        write(&root, "build/drop.txt", "drop");
        write(&root, "build/keep.txt", "keep");

        let walker = NativeWalker::new(vec![root.clone()], NativeWalkOptions::default()).unwrap();
        assert_eq!(walked_names(&walker), ["keep.txt"]);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn linked_worktree_common_exclude_is_loaded_relative_to_gitdir() {
        let root = temp_directory("linked-worktree-exclude");
        let worktree = root.join("worktree");
        let git_dir = root.join("git-dir");
        let common_dir = root.join("common-dir");
        fs::create_dir_all(git_dir.join("info")).expect("create git info");
        fs::create_dir_all(common_dir.join("info")).expect("create common info");
        fs::create_dir_all(&worktree).expect("create worktree");
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .expect("write git file");
        fs::write(
            git_dir.join("commondir"),
            format!("{}\n", PathBuf::from("..").join("common-dir").display()),
        )
        .expect("write commondir");
        fs::write(
            common_dir.join("info").join("exclude"),
            "common-only.txt\nshared.txt\n",
        )
        .expect("write common exclude");
        fs::write(
            git_dir.join("info").join("exclude"),
            "local-only.txt\n!shared.txt\n",
        )
        .expect("write worktree exclude");
        write(&worktree, "common-only.txt", "common");
        write(&worktree, "local-only.txt", "local");
        write(&worktree, "shared.txt", "shared");
        write(&worktree, "visible.txt", "visible");

        let walker = NativeWalker::new(vec![worktree], NativeWalkOptions::default()).unwrap();
        assert_eq!(walked_names(&walker), ["shared.txt", "visible.txt"]);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn glob_engine_handles_recursive_and_class_patterns() {
        assert!(glob_matches("src/**/lib?.[rs]", "src/a/b/libx.r"));
        assert!(glob_matches("**/*.txt", "note.txt"));
        assert!(glob_matches("**/*.txt", "nested/note.txt"));
        assert!(!glob_matches("*.txt", "nested/note.txt"));
        assert!(glob_matches("*資料.txt", "日本語資料.txt"));
        assert!(glob_matches(&"*a".repeat(32), &"a".repeat(96)));
        assert!(parse_user_glob("*.rs").is_ok());
        assert!(parse_user_glob("[unterminated").is_err());
        assert!(parse_user_glob(&format!("{}*", "a".repeat(2048))).is_ok());
        assert!(parse_user_glob(&"**".repeat(513)).is_err());
    }

    #[test]
    fn user_glob_expands_braces_and_preserves_escaped_meta() {
        let root = temp_directory("user-brace-glob");
        write(&root, "lib.rs", "rust");
        write(&root, "Cargo.toml", "toml");
        write(&root, "note.txt", "text");
        write(&root, "[literal].txt", "literal");

        let braces = NativeWalker::new(
            vec![root.clone()],
            NativeWalkOptions {
                globs: vec!["*.{rs,toml}".to_owned()],
                ..NativeWalkOptions::default()
            },
        )
        .unwrap();
        assert_eq!(walked_names(&braces), ["Cargo.toml", "lib.rs"]);

        let escaped = NativeWalker::new(
            vec![root.clone()],
            NativeWalkOptions {
                globs: vec![r"\[literal\].txt".to_owned()],
                ..NativeWalkOptions::default()
            },
        )
        .unwrap();
        assert_eq!(walked_names(&escaped), ["[literal].txt"]);

        let too_many = format!(
            "{{{}}}",
            (0..=super::MAX_BRACE_EXPANSIONS)
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(parse_user_glob(&too_many).is_err());
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn parallel_walk_uses_one_reusable_visitor_per_worker() {
        let root = temp_directory("parallel");
        for index in 0..200 {
            write(&root, &format!("tree/{index}.txt"), "content");
        }
        let walker = NativeWalker::new(vec![root.clone()], NativeWalkOptions::default()).unwrap();
        let count = AtomicUsize::new(0);
        walker.run_parallel(4, || {
            let count = &count;
            move |entry| {
                if entry.is_ok() {
                    count.fetch_add(1, Ordering::Relaxed);
                }
                WalkControl::Continue
            }
        });
        assert_eq!(count.load(Ordering::Relaxed), 200);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn visitor_panic_stops_and_wakes_parallel_workers() {
        let root = temp_directory("parallel-panic");
        for index in 0..200 {
            write(&root, &format!("tree/{index}.txt"), "content");
        }
        let walker = NativeWalker::new(vec![root.clone()], NativeWalkOptions::default()).unwrap();
        let did_panic = AtomicBool::new(false);
        let result = catch_unwind(AssertUnwindSafe(|| {
            walker.run_parallel(4, || {
                let did_panic = &did_panic;
                move |entry| {
                    if entry.is_ok() && !did_panic.swap(true, Ordering::AcqRel) {
                        panic!("intentional visitor panic");
                    }
                    WalkControl::Continue
                }
            });
        }));
        assert!(result.is_err());
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn preserves_the_relative_root_spelling_in_emitted_paths() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let root_name = format!(".fastfs-native-walker-display-{nonce}");
        let root = PathBuf::from(&root_name);
        write(&root, "nested/file.txt", "content");

        let input = PathBuf::from(".").join(&root_name);
        let walker = NativeWalker::new(vec![input.clone()], NativeWalkOptions::default()).unwrap();
        let entries = walker
            .build()
            .collect::<Result<Vec<_>, _>>()
            .expect("walk success");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), input.join("nested").join("file.txt"));
        assert!(entries[0].filesystem_path().is_absolute());
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn lexical_parent_root_keeps_display_and_matches_cwd_relative_glob() {
        let container = temp_directory("lexical-parent-root");
        let current_dir = container.join("cwd");
        let sibling = container.join("sibling");
        fs::create_dir_all(&current_dir).expect("create logical cwd");
        write(&sibling, ".rgignore", "ignored.rs\n");
        write(&sibling, "ignored.rs", "ignored");
        write(&sibling, "src/keep.rs", "keep");
        write(&sibling, "src/skip.txt", "skip");
        let input = PathBuf::from("..").join("sibling");

        let default = NativeWalker::new(
            vec![input.clone()],
            NativeWalkOptions {
                current_dir: Some(current_dir.clone()),
                ..NativeWalkOptions::default()
            },
        )
        .unwrap();
        assert_eq!(walked_names(&default), ["keep.rs", "skip.txt"]);

        let globbed = NativeWalker::new(
            vec![input.clone()],
            NativeWalkOptions {
                current_dir: Some(current_dir),
                globs: vec!["../sibling/src/*.rs".to_owned()],
                ..NativeWalkOptions::default()
            },
        )
        .unwrap();
        let entries = globbed
            .build()
            .collect::<Result<Vec<_>, _>>()
            .expect("walk success");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), input.join("src").join("keep.rs"));
        assert_eq!(entries[0].filesystem_path(), sibling.join("src/keep.rs"));
        assert!(
            entries[0]
                .filesystem_path()
                .components()
                .all(|component| !matches!(component, Component::CurDir | Component::ParentDir))
        );
        fs::remove_dir_all(container).expect("remove test directory");
    }
}
