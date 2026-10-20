use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    GetFullPathNameW, MoveFileExW, ReOpenFile, SetFileAttributesW, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_DELETE_ON_CLOSE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING,
};

use crate::util;

// Match the path preparation used by Rust's Windows filesystem APIs. Creating a
// temporary file already uses those APIs; publishing it must accept the same
// paths, independently of the executable manifest and LongPathsEnabled policy.
fn to_utf16(path: &Path) -> io::Result<Vec<u16>> {
    let mut path: Vec<u16> = path.as_os_str().encode_wide().collect();
    if path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a nul byte",
        ));
    }
    path.push(0);
    const SEP: u16 = b'\\' as u16;
    const ALT: u16 = b'/' as u16;
    const COLON: u16 = b':' as u16;
    const DOT: u16 = b'.' as u16;
    const QUERY: u16 = b'?' as u16;
    const VERBATIM: &[u16] = &[SEP, SEP, QUERY, SEP];
    const NT: &[u16] = &[SEP, QUERY, QUERY, SEP];
    const UNC: &[u16] = &[
        SEP,
        SEP,
        QUERY,
        SEP,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        SEP,
    ];
    // 248 also covers Win32 directory operations. Keep existing short absolute
    // paths unchanged; a short relative path may still resolve to a long one.
    const LEGACY_LIMIT: usize = 248;
    if path.starts_with(VERBATIM) || path.starts_with(NT) || path == [0] {
        return Ok(path);
    }
    if path.len() < LEGACY_LIMIT {
        match path.as_slice() {
            [drive, COLON, 0] | [drive, COLON, SEP | ALT, ..] if *drive != SEP && *drive != ALT => {
                return Ok(path)
            }
            [SEP | ALT, SEP | ALT, ..] => return Ok(path),
            _ => {}
        }
    }

    let mut absolute = vec![0; 260];
    loop {
        // SAFETY: input is NUL terminated; output points to a writable buffer
        // of the declared length. GetFullPathNameW does not retain either.
        let len = unsafe {
            GetFullPathNameW(
                path.as_ptr(),
                absolute.len() as u32,
                absolute.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if (len as usize) < absolute.len() {
            absolute.truncate(len as usize);
            break;
        }
        // An insufficient-buffer result includes space for the trailing NUL.
        absolute.resize(len as usize, 0);
    }
    let mut normalized = absolute.as_slice();
    path.clear();
    if normalized.len() + 1 >= LEGACY_LIMIT {
        // GetFullPathNameW has resolved relative components and converted '/'
        // to '\\'. Prefixing the original input would change those semantics.
        let prefix = match normalized {
            [_, COLON, SEP, ..] => VERBATIM,
            [SEP, SEP, DOT, SEP, ..] => {
                normalized = &normalized[4..];
                VERBATIM
            }
            [SEP, SEP, QUERY, SEP, ..] | [SEP, QUERY, QUERY, SEP, ..] => &[],
            [SEP, SEP, ..] => {
                normalized = &normalized[2..];
                UNC
            }
            _ => &[],
        };
        path.extend_from_slice(prefix);
    }
    path.extend_from_slice(normalized);
    path.push(0);
    Ok(path)
}

fn not_supported<T>(msg: &str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Other, msg))
}

pub fn create_named(
    path: &Path,
    open_options: &mut OpenOptions,
    permissions: Option<&std::fs::Permissions>,
) -> io::Result<File> {
    if permissions.map_or(false, |p| p.readonly()) {
        return not_supported("changing permissions is not supported on this platform");
    }
    open_options
        .create_new(true)
        .read(true)
        .write(true)
        .custom_flags(FILE_ATTRIBUTE_TEMPORARY)
        .open(path)
}

pub fn create(dir: &Path) -> io::Result<File> {
    util::create_helper(
        dir,
        OsStr::new(".tmp"),
        OsStr::new(""),
        crate::NUM_RAND_CHARS,
        |path| {
            let f = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .share_mode(0)
                .custom_flags(FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_DELETE_ON_CLOSE)
                .open(path)?;
            // NOTE: in theory, we could delete the file immediately (we open the file in "unix
            // semantics" mode) but this seemed to corrupt something in Windows at scale (see #339).
            // So we just rely on `FILE_FLAG_DELETE_ON_CLOSE`.
            Ok(f)
        },
    )
}

pub fn reopen(file: &File, _path: &Path) -> io::Result<File> {
    let handle = file.as_raw_handle();
    unsafe {
        let handle = ReOpenFile(
            handle as HANDLE,
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_DELETE | FILE_SHARE_READ | FILE_SHARE_WRITE,
            0,
        );
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(FromRawHandle::from_raw_handle(handle as RawHandle))
        }
    }
}

pub fn keep(path: &Path) -> io::Result<()> {
    unsafe {
        let path_w = to_utf16(path)?;
        if SetFileAttributesW(path_w.as_ptr(), FILE_ATTRIBUTE_NORMAL) == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

pub fn persist(old_path: &Path, new_path: &Path, overwrite: bool) -> io::Result<()> {
    unsafe {
        let old_path_w = to_utf16(old_path)?;
        let new_path_w = to_utf16(new_path)?;

        // Don't succeed if this fails. We don't want to claim to have successfully persisted a file
        // still marked as temporary because this file won't have the same consistency guarantees.
        if SetFileAttributesW(old_path_w.as_ptr(), FILE_ATTRIBUTE_NORMAL) == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut flags = 0;

        if overwrite {
            flags |= MOVEFILE_REPLACE_EXISTING;
        }

        if MoveFileExW(old_path_w.as_ptr(), new_path_w.as_ptr(), flags) == 0 {
            let e = io::Error::last_os_error();
            // If this fails, the temporary file is now un-hidden and no longer marked temporary
            // (slightly less efficient) but it will still work.
            let _ = SetFileAttributesW(old_path_w.as_ptr(), FILE_ATTRIBUTE_TEMPORARY);
            Err(e)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    fn prepared(path: &str) -> String {
        let wide = to_utf16(Path::new(path)).unwrap();
        String::from_utf16(&wide[..wide.len() - 1]).unwrap()
    }

    #[test]
    fn long_path_normalizes_before_adding_namespace() {
        let tail = "x".repeat(260);
        assert_eq!(
            prepared(&format!("C:/old/../new/{tail}")),
            format!("\\\\?\\C:\\new\\{tail}")
        );
        assert_eq!(
            prepared(&format!("//server/share/old/../new/{tail}")),
            format!("\\\\?\\UNC\\server\\share\\new\\{tail}")
        );
    }

    #[test]
    fn native_namespace_and_utf16_are_preserved() {
        for prefix in [r"\\?\C:\", r"\??\C:\", r"\\?\UNC\server\share\"] {
            let input = format!("{prefix}{}", "文".repeat(260));
            assert_eq!(prepared(&input), input);
        }
        let mut wide: Vec<u16> = r"C:\".encode_utf16().collect();
        wide.extend(std::iter::repeat(0xD800).take(260));
        let input = Path::new(&OsString::from_wide(&wide)).to_owned();
        let actual = to_utf16(&input).unwrap();
        let mut expected: Vec<u16> = r"\\?\".encode_utf16().collect();
        expected.extend(wide);
        expected.push(0);
        assert_eq!(actual, expected);
    }

    #[test]
    fn nul_rejected_without_truncation() {
        assert_eq!(
            to_utf16(Path::new("C:\\valid\0suffix")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
