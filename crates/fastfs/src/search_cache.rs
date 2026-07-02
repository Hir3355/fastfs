use std::collections::BTreeSet;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf, Prefix};

const MAX_IGNORE_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct IgnoreSnapshot {
    files: Vec<IgnoreFileSnapshot>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct IgnoreFileSnapshot {
    path: PathBuf,
    value: IgnoreFileValue,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum IgnoreFileValue {
    Contents(Vec<u8>),
    Error(io::ErrorKind, Option<i32>),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct FileListCacheKey {
    input_roots: Vec<PathBuf>,
    canonical_roots: Vec<PathBuf>,
    current_dir: PathBuf,
    hidden: bool,
    no_ignore: bool,
    globs: Vec<String>,
    ignore_snapshot: IgnoreSnapshot,
}

impl FileListCacheKey {
    pub(crate) fn new(
        roots: &[PathBuf],
        current_dir: &Path,
        hidden: bool,
        no_ignore: bool,
        follow: bool,
        globs: &[String],
    ) -> Option<Self> {
        if follow || roots.is_empty() {
            return None;
        }
        let mut canonical_roots = Vec::with_capacity(roots.len());
        for root in roots {
            let canonical = std::fs::canonicalize(root).ok()?;
            if !canonical.is_dir() || is_unc_or_device_path(&canonical) {
                return None;
            }
            canonical_roots.push(canonical);
        }
        if !crate::search_platform::roots_are_local_fixed(&canonical_roots) {
            return None;
        }
        let ignore_snapshot = capture_ignore_snapshot(&canonical_roots, no_ignore)?;
        Some(Self {
            input_roots: roots.to_vec(),
            ignore_snapshot,
            canonical_roots,
            current_dir: std::fs::canonicalize(current_dir).ok()?,
            hidden,
            no_ignore,
            globs: globs.to_vec(),
        })
    }

    pub(crate) fn roots(&self) -> &[PathBuf] {
        &self.canonical_roots
    }

    fn ignore_snapshot_is_current(&self) -> bool {
        capture_ignore_snapshot(&self.canonical_roots, self.no_ignore).as_ref()
            == Some(&self.ignore_snapshot)
    }
}

fn capture_ignore_snapshot(roots: &[PathBuf], no_ignore: bool) -> Option<IgnoreSnapshot> {
    if no_ignore {
        return Some(IgnoreSnapshot { files: Vec::new() });
    }

    let mut candidates = BTreeSet::new();
    for root in roots {
        for ancestor in root.ancestors() {
            candidates.insert(ancestor.join(".ignore"));
            candidates.insert(ancestor.join(".gitignore"));
            candidates.insert(ancestor.join(".rgignore"));
            let git_file = ancestor.join(".git");
            candidates.insert(git_file.clone());
            candidates.insert(ancestor.join(".jj"));
            candidates.insert(git_file.join("info").join("exclude"));
            if git_file.is_file()
                && let Ok(contents) = std::fs::read_to_string(&git_file)
                && let Some(git_dir) = contents
                    .lines()
                    .next()
                    .and_then(|line| line.strip_prefix("gitdir: "))
            {
                let git_dir = PathBuf::from(git_dir);
                add_worktree_ignore_candidates(&git_dir, &mut candidates);
                let resolved = if git_dir.is_absolute() {
                    git_dir
                } else {
                    ancestor.join(&git_dir)
                };
                add_worktree_ignore_candidates(&resolved, &mut candidates);
            }
        }
    }
    if let Some(global) = crate::native_walker::global_git_ignore_path() {
        candidates.insert(global);
    }

    let mut files = Vec::with_capacity(candidates.len());
    let mut total_bytes = 0_usize;
    for candidate in candidates {
        let value = match read_snapshot_file(&candidate) {
            Ok(contents) => {
                total_bytes = total_bytes.checked_add(contents.len())?;
                if total_bytes > MAX_IGNORE_SNAPSHOT_BYTES {
                    return None;
                }
                IgnoreFileValue::Contents(contents)
            }
            Err(error) => IgnoreFileValue::Error(error.kind(), error.raw_os_error()),
        };
        files.push(IgnoreFileSnapshot {
            path: candidate,
            value,
        });
    }
    Some(IgnoreSnapshot { files })
}

fn read_snapshot_file(path: &Path) -> io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut contents = Vec::new();
    file.take((MAX_IGNORE_SNAPSHOT_BYTES as u64) + 1)
        .read_to_end(&mut contents)?;
    if contents.len() > MAX_IGNORE_SNAPSHOT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ignore snapshot file is too large",
        ));
    }
    Ok(contents)
}

fn add_worktree_ignore_candidates(git_dir: &Path, candidates: &mut BTreeSet<PathBuf>) {
    candidates.insert(git_dir.join("info").join("exclude"));
    let commondir_file = git_dir.join("commondir");
    candidates.insert(commondir_file.clone());
    let Ok(contents) = std::fs::read_to_string(&commondir_file) else {
        return;
    };
    let Some(commondir) = contents.lines().next() else {
        return;
    };
    let commondir = if commondir.starts_with('.') {
        git_dir.join(commondir)
    } else {
        PathBuf::from(commondir)
    };
    candidates.insert(commondir.join("info").join("exclude"));
}

fn is_unc_or_device_path(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(
                prefix.kind(),
                Prefix::UNC(..) | Prefix::VerbatimUNC(..) | Prefix::DeviceNS(..)
            )
    )
}

#[cfg(windows)]
mod platform {
    use super::FileListCacheKey;
    use std::collections::{HashMap, HashSet};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, OnceLock};
    use windows_sys::Win32::Foundation::{TRUE, WAIT_TIMEOUT};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
        FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    const MIN_PATHS_PER_ENTRY: usize = if cfg!(test) { 1 } else { 1024 };
    const MAX_CACHE_ENTRIES: usize = 4;
    const MAX_PATHS_PER_ENTRY: usize = 300_000;
    const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
    const WATCH_BUFFER_U32S: usize = (64 * 1024) / std::mem::size_of::<u32>();

    pub(crate) struct CacheBuild {
        key: FileListCacheKey,
        root_identities: Vec<RootIdentity>,
        watches: Vec<DirectoryWatch>,
    }

    struct CacheEntry {
        paths: Arc<[PathBuf]>,
        approximate_bytes: usize,
        root_identities: Vec<RootIdentity>,
        watches: Vec<DirectoryWatch>,
        last_used: u64,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct RootIdentity {
        volume_serial_number: u32,
        file_index: u64,
    }

    fn root_identities(roots: &[PathBuf]) -> Option<Vec<RootIdentity>> {
        roots.iter().map(|root| root_identity(root)).collect()
    }

    fn root_identity(root: &Path) -> Option<RootIdentity> {
        let directory = OpenOptions::new()
            .read(true)
            .access_mode(FILE_LIST_DIRECTORY)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(root)
            .ok()?;
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(directory.as_raw_handle(), &mut information) } == 0 {
            return None;
        }
        Some(RootIdentity {
            volume_serial_number: information.dwVolumeSerialNumber,
            file_index: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
        })
    }

    struct DirectoryWatch {
        directory: File,
        event: OwnedHandle,
        overlapped: Box<OVERLAPPED>,
        _buffer: Box<[u32]>,
    }

    // SAFETY: Windows file/event handles and the pending OVERLAPPED operation may be
    // completed or cancelled from a different thread. The pointed-to OVERLAPPED and
    // buffer stay pinned in their boxes, and cache access serializes status checks/drop.
    unsafe impl Send for DirectoryWatch {}

    impl DirectoryWatch {
        fn new(path: &Path, recursive: bool) -> io::Result<Self> {
            let directory = OpenOptions::new()
                .read(true)
                .access_mode(FILE_LIST_DIRECTORY)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED)
                .open(path)?;
            let event_handle = unsafe { CreateEventW(std::ptr::null(), TRUE, 0, std::ptr::null()) };
            if event_handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let event = unsafe { OwnedHandle::from_raw_handle(event_handle) };
            let mut overlapped = Box::new(OVERLAPPED::default());
            overlapped.hEvent = event.as_raw_handle();
            let mut buffer = vec![0_u32; WATCH_BUFFER_U32S].into_boxed_slice();
            let queued = unsafe {
                ReadDirectoryChangesW(
                    directory.as_raw_handle(),
                    buffer.as_mut_ptr().cast(),
                    std::mem::size_of_val(buffer.as_ref()) as u32,
                    i32::from(recursive),
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_ATTRIBUTES
                        | FILE_NOTIFY_CHANGE_LAST_WRITE,
                    std::ptr::null_mut(),
                    overlapped.as_mut(),
                    None,
                )
            };
            if queued == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                directory,
                event,
                overlapped,
                _buffer: buffer,
            })
        }

        fn is_dirty(&self) -> bool {
            (unsafe { WaitForSingleObject(self.event.as_raw_handle(), 0) }) != WAIT_TIMEOUT
        }
    }

    impl Drop for DirectoryWatch {
        fn drop(&mut self) {
            unsafe {
                let _ = CancelIoEx(self.directory.as_raw_handle(), self.overlapped.as_ref());
                let mut transferred = 0_u32;
                let _ = GetOverlappedResult(
                    self.directory.as_raw_handle(),
                    self.overlapped.as_ref(),
                    &mut transferred,
                    TRUE,
                );
            }
        }
    }

    impl CacheBuild {
        fn is_dirty(&self) -> bool {
            self.watches.iter().any(DirectoryWatch::is_dirty)
        }

        fn roots_are_current(&self) -> bool {
            root_identities(self.key.roots()).as_ref() == Some(&self.root_identities)
        }
    }

    impl CacheEntry {
        fn is_dirty(&self) -> bool {
            self.watches.iter().any(DirectoryWatch::is_dirty)
        }

        fn roots_are_current(&self, identities: &[RootIdentity]) -> bool {
            self.root_identities == identities
        }
    }

    #[derive(Default)]
    struct CacheState {
        entries: HashMap<FileListCacheKey, CacheEntry>,
        clock: u64,
        total_bytes: usize,
    }

    static CACHE: OnceLock<Mutex<CacheState>> = OnceLock::new();

    fn cache() -> &'static Mutex<CacheState> {
        CACHE.get_or_init(|| Mutex::new(CacheState::default()))
    }

    pub(crate) fn lookup(key: &FileListCacheKey) -> Option<Arc<[PathBuf]>> {
        if !key.ignore_snapshot_is_current() {
            return None;
        }
        let identities = root_identities(key.roots())?;
        let removed = {
            let mut state = cache().lock().unwrap_or_else(|error| error.into_inner());
            if state
                .entries
                .get(key)
                .is_some_and(|entry| entry.is_dirty() || !entry.roots_are_current(&identities))
            {
                let removed = state.entries.remove(key);
                if let Some(entry) = &removed {
                    state.total_bytes = state.total_bytes.saturating_sub(entry.approximate_bytes);
                }
                Some(removed)
            } else {
                state.clock = state.clock.wrapping_add(1);
                let clock = state.clock;
                let entry = state.entries.get_mut(key)?;
                entry.last_used = clock;
                return Some(Arc::clone(&entry.paths));
            }
        };
        drop(removed);
        None
    }

    pub(crate) fn begin(key: FileListCacheKey) -> Option<CacheBuild> {
        let mut watches = Vec::new();
        let mut recursively_watched = Vec::<PathBuf>::new();
        for root in key.roots() {
            if recursively_watched
                .iter()
                .any(|parent| root.starts_with(parent))
            {
                continue;
            }
            watches.push(DirectoryWatch::new(root, true).ok()?);
            recursively_watched.push(root.clone());
        }

        let mut watched_ancestors = HashSet::new();
        for root in key.roots() {
            let parent = root.parent()?;
            if watched_ancestors.insert(parent.to_path_buf()) {
                // This covers replacement of the watched root itself. Ignore files
                // outside the root are validated from exact snapshots at commit/lookup.
                watches.push(DirectoryWatch::new(parent, false).ok()?);
            }
        }

        let root_identities = root_identities(key.roots())?;
        Some(CacheBuild {
            key,
            root_identities,
            watches,
        })
    }

    pub(crate) fn commit(build: CacheBuild, paths: Vec<PathBuf>) {
        if paths.len() < MIN_PATHS_PER_ENTRY
            || paths.len() > MAX_PATHS_PER_ENTRY
            || build.is_dirty()
            || !build.key.ignore_snapshot_is_current()
            || !build.roots_are_current()
        {
            return;
        }

        let approximate_bytes = paths.iter().fold(0_usize, |total, path| {
            total.saturating_add(std::mem::size_of::<PathBuf>() + path.as_os_str().len())
        });
        if approximate_bytes > MAX_TOTAL_BYTES {
            return;
        }
        if build.is_dirty() || !build.key.ignore_snapshot_is_current() || !build.roots_are_current()
        {
            return;
        }
        let paths: Arc<[PathBuf]> = paths.into();
        let mut removed = Vec::new();
        {
            let mut state = cache().lock().unwrap_or_else(|error| error.into_inner());
            state.clock = state.clock.wrapping_add(1);
            let clock = state.clock;
            if let Some(previous) = state.entries.remove(&build.key) {
                state.total_bytes = state.total_bytes.saturating_sub(previous.approximate_bytes);
                removed.push(previous);
            }
            state.total_bytes += approximate_bytes;
            state.entries.insert(
                build.key,
                CacheEntry {
                    paths,
                    approximate_bytes,
                    root_identities: build.root_identities,
                    watches: build.watches,
                    last_used: clock,
                },
            );

            while state.entries.len() > MAX_CACHE_ENTRIES || state.total_bytes > MAX_TOTAL_BYTES {
                let Some(oldest_key) = state
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                if let Some(entry) = state.entries.remove(&oldest_key) {
                    state.total_bytes = state.total_bytes.saturating_sub(entry.approximate_bytes);
                    removed.push(entry);
                }
            }
        }
        drop(removed);
    }

    #[cfg(test)]
    pub(crate) fn clear() {
        let removed = {
            let mut state = cache().lock().unwrap_or_else(|error| error.into_inner());
            state.total_bytes = 0;
            state
                .entries
                .drain()
                .map(|(_, entry)| entry)
                .collect::<Vec<_>>()
        };
        drop(removed);
    }

    #[cfg(test)]
    pub(crate) fn detach_watches(key: &FileListCacheKey) {
        let watches = {
            let mut state = cache().lock().unwrap_or_else(|error| error.into_inner());
            state
                .entries
                .get_mut(key)
                .map(|entry| std::mem::take(&mut entry.watches))
        };
        drop(watches);
    }
}

#[cfg(not(windows))]
mod platform {
    use super::FileListCacheKey;
    use std::path::PathBuf;
    use std::sync::Arc;

    pub(crate) struct CacheBuild;

    pub(crate) fn lookup(_key: &FileListCacheKey) -> Option<Arc<[PathBuf]>> {
        None
    }

    pub(crate) fn begin(_key: FileListCacheKey) -> Option<CacheBuild> {
        None
    }

    pub(crate) fn commit(_build: CacheBuild, _paths: Vec<PathBuf>) {}
}

pub(crate) use platform::{begin, commit, lookup};

#[cfg(all(test, windows))]
mod tests {
    use super::{FileListCacheKey, begin, commit, lookup, platform};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::thread;
    use std::time::{Duration, SystemTime};

    fn temporary_directory() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("fastfs-cache-{}-{unique}", std::process::id()))
            .join("root")
    }

    fn cache_test_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    #[test]
    fn invalidates_cached_paths_after_directory_change() {
        let _guard = cache_test_guard();
        platform::clear();
        let root = temporary_directory();
        fs::create_dir_all(&root).unwrap();
        let original = root.join("original.txt");
        fs::write(&original, b"before").unwrap();
        let key =
            FileListCacheKey::new(std::slice::from_ref(&root), &root, false, false, false, &[])
                .expect("local directory should be cacheable");
        let build = begin(key.clone()).expect("watcher should start");
        commit(build, vec![original]);
        assert!(lookup(&key).is_some());

        fs::write(root.join("added.txt"), b"after").unwrap();
        let invalidated = (0..100).any(|_| {
            if lookup(&key).is_none() {
                true
            } else {
                thread::sleep(Duration::from_millis(20));
                false
            }
        });

        platform::clear();
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
        assert!(
            invalidated,
            "filesystem watcher did not invalidate the cache"
        );
    }

    #[test]
    fn rejects_cached_paths_after_root_ancestor_replacement() {
        let _guard = cache_test_guard();
        platform::clear();
        let root = temporary_directory();
        let container = root.parent().unwrap().to_path_buf();
        let moved_container = container.with_extension("moved");
        fs::create_dir_all(&root).unwrap();
        let original = root.join("original.txt");
        fs::write(&original, b"old tree").unwrap();
        let key =
            FileListCacheKey::new(std::slice::from_ref(&root), &root, false, false, false, &[])
                .unwrap();
        let build = begin(key.clone()).unwrap();
        commit(build, vec![original]);
        assert!(lookup(&key).is_some());

        // Windows can deny an ancestor rename while the test's live directory
        // handles are open. Detaching them simulates a watcher that stayed on
        // the old tree or was otherwise lost; file identity must still reject it.
        platform::detach_watches(&key);
        fs::rename(&container, &moved_container).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("replacement.txt"), b"new tree").unwrap();

        assert!(
            lookup(&key).is_none(),
            "the same path backed by a different directory identity must miss"
        );

        platform::clear();
        fs::remove_dir_all(&container).unwrap();
        fs::remove_dir_all(&moved_container).unwrap();
    }

    #[test]
    fn rejects_commit_after_worktree_common_exclude_changes() {
        let _guard = cache_test_guard();
        platform::clear();
        let root = temporary_directory();
        let container = root.parent().unwrap();
        let git_dir = container.join("git-dir");
        let common_dir = container.join("common-dir");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        fs::create_dir_all(common_dir.join("info")).unwrap();
        fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        fs::write(
            git_dir.join("commondir"),
            format!("{}\n", common_dir.display()),
        )
        .unwrap();
        let exclude = common_dir.join("info").join("exclude");
        fs::write(&exclude, b"before\n").unwrap();
        let original = root.join("original.txt");
        fs::write(&original, b"content").unwrap();

        let key_before =
            FileListCacheKey::new(std::slice::from_ref(&root), &root, false, false, false, &[])
                .unwrap();
        let build = begin(key_before.clone()).unwrap();
        fs::write(&exclude, b"after\n").unwrap();
        let key_after =
            FileListCacheKey::new(std::slice::from_ref(&root), &root, false, false, false, &[])
                .unwrap();

        assert_ne!(
            key_before, key_after,
            "commondir exclude must affect the key"
        );
        commit(build, vec![original]);
        assert!(
            lookup(&key_before).is_none(),
            "a build made with an obsolete ignore snapshot must not be committed"
        );

        platform::clear();
        fs::remove_dir_all(container).unwrap();
    }

    #[test]
    fn lookup_rejects_an_obsolete_ignore_snapshot() {
        let _guard = cache_test_guard();
        platform::clear();
        let root = temporary_directory();
        let container = root.parent().unwrap();
        let git_dir = container.join("git-dir");
        let common_dir = container.join("common-dir");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        fs::create_dir_all(common_dir.join("info")).unwrap();
        fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        fs::write(
            git_dir.join("commondir"),
            format!("{}\n", common_dir.display()),
        )
        .unwrap();
        let exclude = common_dir.join("info").join("exclude");
        fs::write(&exclude, b"before\n").unwrap();
        let original = root.join("original.txt");
        fs::write(&original, b"content").unwrap();

        let key =
            FileListCacheKey::new(std::slice::from_ref(&root), &root, false, false, false, &[])
                .unwrap();
        let build = begin(key.clone()).unwrap();
        commit(build, vec![original]);
        assert!(lookup(&key).is_some());

        fs::write(&exclude, b"after\n").unwrap();
        assert!(
            lookup(&key).is_none(),
            "lookup must revalidate the snapshot carried by an older key"
        );

        platform::clear();
        fs::remove_dir_all(container).unwrap();
    }
}
