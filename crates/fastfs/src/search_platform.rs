use std::fs::File;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(windows)]
pub(crate) fn open_search_file(path: &Path, sequential_hint: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_SEQUENTIAL_SCAN;

    let mut options = OpenOptions::new();
    options.read(true);
    if sequential_hint {
        options.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
    }
    options.open(path)
}

#[cfg(not(windows))]
pub(crate) fn open_search_file(path: &Path, _sequential_hint: bool) -> io::Result<File> {
    File::open(path)
}

pub(crate) const fn sequential_scan_size_probe_enabled() -> bool {
    cfg!(windows)
}

#[cfg(windows)]
pub(crate) fn sequential_scan_is_beneficial(length: u64) -> bool {
    const MINIMUM_LENGTH: u64 = 1024 * 1024;
    length >= MINIMUM_LENGTH
}

#[cfg(not(windows))]
pub(crate) fn sequential_scan_is_beneficial(_length: u64) -> bool {
    false
}

#[cfg(windows)]
pub(crate) fn roots_are_fast_storage(roots: &[PathBuf]) -> bool {
    use std::collections::HashSet;

    if roots.is_empty() || roots.len() > 16 {
        return false;
    }
    let mut volumes = HashSet::new();
    for root in roots {
        let Some(volume) = volume_path(root) else {
            return false;
        };
        volumes.insert(volume);
    }
    volumes.into_iter().all(volume_has_no_seek_penalty)
}

#[cfg(not(windows))]
pub(crate) fn roots_are_fast_storage(_roots: &[PathBuf]) -> bool {
    false
}

#[cfg(windows)]
pub(crate) fn roots_are_local_fixed(roots: &[PathBuf]) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

    const DRIVE_FIXED: u32 = 3;
    const DRIVE_RAMDISK: u32 = 6;

    !roots.is_empty()
        && roots.iter().all(|root| {
            let Some(volume) = volume_path(root) else {
                return false;
            };
            let wide = volume
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            matches!(
                unsafe { GetDriveTypeW(wide.as_ptr()) },
                DRIVE_FIXED | DRIVE_RAMDISK
            )
        })
}

#[cfg(not(windows))]
pub(crate) fn roots_are_local_fixed(_roots: &[PathBuf]) -> bool {
    false
}

#[cfg(windows)]
fn volume_path(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;

    let canonical = std::fs::canonicalize(path).ok()?;
    let wide = canonical
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut output = vec![0_u16; 32_768];
    let success =
        unsafe { GetVolumePathNameW(wide.as_ptr(), output.as_mut_ptr(), output.len() as u32) };
    if success == 0 {
        return None;
    }
    let length = output.iter().position(|&value| value == 0)?;
    Some(std::ffi::OsString::from_wide(&output[..length]).into())
}

#[cfg(windows)]
fn volume_has_no_seek_penalty(volume_root: PathBuf) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(value) = cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&volume_root)
        .copied()
    {
        return value;
    }
    let value = query_volume_has_no_seek_penalty(&volume_root).unwrap_or(false);
    cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(volume_root, value);
    value
}

#[cfg(windows)]
fn query_volume_has_no_seek_penalty(volume_root: &Path) -> Option<bool> {
    use std::ffi::OsString;
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW,
        GetVolumeNameForVolumeMountPointW,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
    };

    const DRIVE_FIXED: u32 = 3;
    const DRIVE_RAMDISK: u32 = 6;

    let root_wide = volume_root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let drive_type = unsafe { GetDriveTypeW(root_wide.as_ptr()) };
    if drive_type == DRIVE_RAMDISK {
        return Some(true);
    }
    if drive_type != DRIVE_FIXED {
        return Some(false);
    }

    let mut volume_name = vec![0_u16; 128];
    let success = unsafe {
        GetVolumeNameForVolumeMountPointW(
            root_wide.as_ptr(),
            volume_name.as_mut_ptr(),
            volume_name.len() as u32,
        )
    };
    if success == 0 {
        return None;
    }
    let length = volume_name.iter().position(|&value| value == 0)?;
    let mut volume_name = OsString::from_wide(&volume_name[..length]);
    while volume_name.to_string_lossy().ends_with(['\\', '/']) {
        let mut wide = volume_name.encode_wide().collect::<Vec<_>>();
        wide.pop();
        volume_name = OsString::from_wide(&wide);
    }

    let file = OpenOptions::new()
        .read(true)
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(PathBuf::from(volume_name))
        .ok()?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut descriptor: DEVICE_SEEK_PENALTY_DESCRIPTOR = unsafe { zeroed() };
    let mut returned = 0_u32;
    let success = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&raw const query).cast(),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            (&raw mut descriptor).cast(),
            size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    let descriptor_size = size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32;
    if success == 0
        || returned < descriptor_size
        || descriptor.Version < descriptor_size
        || descriptor.Size < descriptor_size
    {
        return None;
    }
    Some(!descriptor.IncursSeekPenalty)
}

#[cfg(test)]
mod tests {
    use super::{open_search_file, sequential_scan_is_beneficial};

    #[test]
    fn sequential_scan_requires_a_large_file_on_windows() {
        #[cfg(windows)]
        {
            assert!(!sequential_scan_is_beneficial(1024 * 1024 - 1));
            assert!(sequential_scan_is_beneficial(1024 * 1024));
        }
        #[cfg(not(windows))]
        {
            assert!(!sequential_scan_is_beneficial(u64::MAX));
        }
    }

    #[test]
    fn sequential_file_open_reads_contents() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "fastfs-sequential-open-{}-{}.txt",
            std::process::id(),
            unique
        ));
        std::fs::write(&path, b"content").unwrap();
        let mut file = open_search_file(&path, true).unwrap();
        let mut value = String::new();
        std::io::Read::read_to_string(&mut file, &mut value).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(value, "content");
    }
}
