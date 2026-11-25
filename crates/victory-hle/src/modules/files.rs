//! Virtual drives over host folders: on the console, title storage is a
//! drive of its own, and titles build paths on that assumption.
//!
//! Every title import that takes a path is bound here. A path on a virtual
//! drive (`X:\..`, `X:/..`, `X:`, `\\?\X:\..`) is rewritten to the host
//! folder behind it and handed to the host's own function; any other path
//! goes to the host unchanged. A rewritten narrow path goes to the host's
//! wide twin, so host folders outside the ANSI code page work; never to our
//! wide wrapper, which would rewrite the host path a second time when its
//! drive letter is mounted too. Letters are chosen among those the host
//! does not use, so no host drive is hidden.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::ffi::{CStr, c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar};
use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

use crate::{HleExport, HleModule, hle_export};

/// The host's own function: what the import would have been bound to.
macro_rules! host {
    ($dll:literal, $name:literal: $ty:ty) => {{
        static ADDRESS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let address = *ADDRESS.get_or_init(|| {
            crate::host::function($dll, $name)
                .unwrap_or_else(|| panic!("the host lacks {}!{}", $dll, $name))
        });
        // SAFETY: the host exports this function with this signature.
        unsafe { std::mem::transmute::<usize, $ty>(address) }
    }};
}
pub(crate) use host;

/// Mounted drives: upper-case letter and host root (UTF-16, no trailing
/// separator, no NUL).
static DRIVES: Mutex<Vec<(u16, Vec<u16>)>> = Mutex::new(Vec::new());

fn drives() -> MutexGuard<'static, Vec<(u16, Vec<u16>)>> {
    DRIVES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Gives the host folder `root` a drive letter (the same one if it already
/// has one): the highest letter neither the host nor another mount uses.
pub fn mount(root: &Path) -> Option<char> {
    let wide = root_wide(root);
    let mut drives = drives();
    if let Some(&(letter, _)) = drives.iter().find(|(_, r)| *r == wide) {
        return char::from_u32(letter.into());
    }
    // SAFETY: no arguments.
    let host = unsafe { GetLogicalDrives() };
    let letter = (b'D'..=b'Z').rev().find(|&l| {
        host >> (l - b'A') & 1 == 0 && !drives.iter().any(|(m, _)| *m == u16::from(l))
    })?;
    tracing::info!(drive = %char::from(letter), root = %root.display(), "virtual drive");
    drives.push((letter.into(), wide));
    Some(letter.into())
}

/// Gives the host folder `root` a drive letter the console fixes (titles
/// open their package as `G:\..` and temporary files as `T:\..`). A host
/// drive of that letter is hidden from the title.
pub fn mount_at(letter: char, root: &Path) {
    let letter = letter.to_ascii_uppercase();
    let Some(index) = u32::from(letter)
        .checked_sub(u32::from('A'))
        .filter(|i| *i < 26)
    else {
        return;
    };
    // SAFETY: no arguments.
    let hides_host_drive = unsafe { GetLogicalDrives() } >> index & 1 != 0;
    tracing::info!(drive = %letter, root = %root.display(), hides_host_drive, "virtual drive");
    let mut drives = drives();
    drives.retain(|(l, _)| u32::from(*l) != u32::from(letter));
    drives.push((letter as u16, root_wide(root)));
}

/// `root` as UTF-16, absolute, without trailing separators (a drive root
/// keeps its own).
fn root_wide(root: &Path) -> Vec<u16> {
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_owned());
    let mut wide: Vec<u16> = root.as_os_str().encode_wide().collect();
    while wide.len() > 3 && wide.last().is_some_and(|c| SEPARATORS.contains(c)) {
        wide.pop();
    }
    wide
}

const SEPARATORS: [u16; 2] = [b'\\' as u16, b'/' as u16];
const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];

/// Host paths from this length on take the `\\?\` form, which lifts the
/// host's MAX_PATH limit (248 characters for a directory, 260 for a file).
const LONG: usize = 248;

/// `root` and the title's `tail` as a `\\?\` path. That form turns off the
/// host's own clean-up, so `/` becomes `\` and `.` and `..` are resolved
/// here (`..` stops at the root).
fn verbatim(root: &[u16], tail: &[u16]) -> Vec<u16> {
    let backslash = u16::from(b'\\');
    let mut out = if root.starts_with(&VERBATIM) {
        root.to_vec()
    } else if let Some(unc) = root.strip_prefix(&[backslash; 2][..]) {
        let mut out = VERBATIM.to_vec();
        out.extend("UNC\\".encode_utf16());
        out.extend_from_slice(unc);
        out
    } else {
        [&VERBATIM[..], root].concat()
    };
    while out.last() == Some(&backslash) {
        out.pop();
    }
    let dot = u16::from(b'.');
    let mut parts: Vec<&[u16]> = Vec::new();
    for part in tail.split(|c| SEPARATORS.contains(c)) {
        match part {
            [] => {}
            [d] if *d == dot => {}
            [d, e] if [*d, *e] == [dot; 2] => drop(parts.pop()),
            _ => parts.push(part),
        }
    }
    for part in parts {
        out.push(backslash);
        out.extend_from_slice(part);
    }
    out
}

/// The host path, NUL-terminated, for `path` (without its NUL) if it is on
/// a virtual drive.
pub(crate) fn translate(path: &[u16]) -> Option<Vec<u16>> {
    let (prefix, rest) = match path.strip_prefix(&VERBATIM[..]) {
        Some(rest) => (&VERBATIM[..], rest),
        None => (&[][..], path),
    };
    let &[letter, colon, ..] = rest else {
        return None;
    };
    if colon != u16::from(b':') {
        return None;
    }
    let letter = u8::try_from(letter).ok().filter(u8::is_ascii_alphabetic)?;
    let letter = u16::from(letter.to_ascii_uppercase());
    let drives = drives();
    let (_, root) = drives.iter().find(|(l, _)| *l == letter)?;
    let tail = &rest[2..];
    if prefix.is_empty() && root.len() + tail.len() >= LONG {
        let mut out = verbatim(root, tail);
        out.push(0);
        return Some(out);
    }
    let mut out = prefix.to_vec();
    out.extend_from_slice(root);
    // `X:` and `X:name` are relative to the root.
    if !tail.first().is_some_and(|c| SEPARATORS.contains(c)) {
        out.push(b'\\'.into());
    }
    let verbatim = !prefix.is_empty();
    out.extend(tail.iter().map(|&c| {
        if verbatim && c == u16::from(b'/') {
            b'\\'.into()
        } else {
            c
        }
    }));
    out.push(0);
    Some(out)
}

/// A title's wide path: the host path when it is on a virtual drive, else
/// the title's own pointer.
struct Wide(*const u16, Option<Vec<u16>>);

impl Wide {
    fn ptr(&self) -> *const u16 {
        self.1.as_ref().map_or(self.0, |p| p.as_ptr())
    }
}

fn wide(path: *const u16) -> Wide {
    if path.is_null() {
        return Wide(path, None);
    }
    // SAFETY: the title passes a NUL-terminated path.
    let path_slice = unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        std::slice::from_raw_parts(path, len)
    };
    let host = translate(path_slice);
    trace(path_slice, host.as_deref());
    Wide(path, host)
}

/// Every title path at debug level, with the host path if rewritten.
fn trace(path: &[u16], host: Option<&[u16]>) {
    tracing::debug!(
        path = %String::from_utf16_lossy(path),
        host = host.map(|h| String::from_utf16_lossy(&h[..h.len() - 1])),
        "title path"
    );
}

/// A narrow path (ANSI code page, as the host decodes it) on a virtual
/// drive, as a wide host path.
fn narrow(path: *const c_char) -> Option<Vec<u16>> {
    if path.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated path.
    let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
    // The drive part is ASCII in every code page; decode only candidates.
    match bytes.strip_prefix(br"\\?\").unwrap_or(bytes) {
        [letter, b':', ..] if letter.is_ascii_alphabetic() => {}
        _ => {
            tracing::debug!(path = %String::from_utf8_lossy(bytes), "title path");
            return None;
        }
    }
    let len = i32::try_from(bytes.len()).ok()?;
    // SAFETY: sizing call, then a call with room for `n` units.
    let wide = unsafe {
        let n = MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0);
        let mut wide = vec![0u16; usize::try_from(n).ok()?];
        MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, wide.as_mut_ptr(), n);
        wide
    };
    let host = translate(&wide);
    trace(&wide, host.as_deref());
    host
}

/// An ASCII mode string (`"rb"`, `"w, ccs=UTF-8"`) for a wide CRT call.
fn widen(mode: *const c_char) -> Vec<u16> {
    if mode.is_null() {
        return vec![0];
    }
    // SAFETY: a NUL-terminated mode string.
    let bytes = unsafe { CStr::from_ptr(mode) }.to_bytes();
    bytes.iter().map(|&b| u16::from(b)).chain([0]).collect()
}

type Handle = *mut c_void;
type File = *mut c_void;

// The package is read-only.
//
// On the console the title's package drive cannot be written. Here it is
// the user's own copy of the game, so the same rule protects it: opening
// for write, creating, truncating, deleting, renaming, or changing the
// attributes or times of anything in a protected folder fails with access
// denied, whether the title names it through a virtual drive or by its host
// path (`GetModuleFileNameW` hands out the real one). Reads are untouched.

/// Protected host folders: absolute, lower-case, `\`-separated, no
/// trailing separator.
static READ_ONLY: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Makes `root` and everything under it read-only to the title.
pub fn protect(root: &Path) {
    let root = if root.as_os_str().is_empty() {
        Path::new(".")
    } else {
        root
    };
    let key = normalized(&root.to_string_lossy());
    tracing::info!(root = %root.display(), "read-only to the title");
    let mut list = READ_ONLY.lock().unwrap_or_else(|e| e.into_inner());
    if !list.contains(&key) {
        list.push(key);
    }
}

/// `path` absolute (`.` and `..` resolved by the host), lower-case,
/// `\`-separated, without a `\\?\` prefix or trailing separators.
fn normalized(path: &str) -> String {
    let path = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
    };
    let path = path.replace('/', "\\");
    let absolute = std::path::absolute(Path::new(&path))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(path);
    let mut key = absolute.to_lowercase();
    while key.len() > 3 && key.ends_with('\\') {
        key.pop();
    }
    key
}

fn in_read_only(path: &str) -> bool {
    let list = READ_ONLY.lock().unwrap_or_else(|e| e.into_inner());
    if list.is_empty() || path.is_empty() {
        return false;
    }
    let path = normalized(path);
    list.iter().any(|root| {
        path == *root
            || path
                .strip_prefix(root.as_str())
                .is_some_and(|rest| rest.starts_with('\\') || root.ends_with('\\'))
    })
}

/// Whether the wide host path (NUL-terminated) is in a protected folder.
fn read_only_wide(path: *const u16) -> bool {
    if path.is_null() {
        return false;
    }
    // SAFETY: a NUL-terminated path.
    let path = unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        std::slice::from_raw_parts(path, len)
    };
    in_read_only(&String::from_utf16_lossy(path))
}

/// Whether the narrow host path (NUL-terminated) is in a protected folder.
fn read_only_narrow(path: *const c_char) -> bool {
    if path.is_null() {
        return false;
    }
    // SAFETY: a NUL-terminated path.
    in_read_only(&unsafe { CStr::from_ptr(path) }.to_string_lossy())
}

const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_ALREADY_EXISTS: u32 = 183;
const EACCES: i32 = 13;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

fn set_last_error(code: u32) {
    host!("kernel32.dll", "SetLastError": extern "system" fn(u32))(code);
}

fn set_errno(code: i32) {
    let errno = host!("ucrtbase.dll", "_errno": extern "C" fn() -> *mut i32)();
    if !errno.is_null() {
        // SAFETY: the CRT's per-thread errno.
        unsafe { *errno = code };
    }
}

fn refused(call: &str) {
    tracing::warn!(call, "title write into the read-only package refused");
}

const GENERIC_READ: u32 = 0x8000_0000;
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
/// Access that can change a file: generic write and all, write data,
/// append, extended attributes, attributes, delete, DAC, owner.
const WRITE_ACCESS: u32 = 0x4000_0000
    | 0x1000_0000
    | 0x0002
    | 0x0004
    | 0x0010
    | 0x0100
    | 0x0001_0000
    | 0x0004_0000
    | 0x0008_0000;
const CREATE_NEW: u32 = 1;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const OPEN_ALWAYS: u32 = 4;
const TRUNCATE_EXISTING: u32 = 5;
const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;

/// A `CreateFile*` request on `host`, made safe for a protected folder:
/// the access and disposition to pass on, or `None` (refused, last error
/// set). `OPEN_ALWAYS` opens only what exists; `MAXIMUM_ALLOWED` reads.
fn guard_open(
    call: &str,
    host: *const u16,
    access: u32,
    disposition: u32,
    flags: u32,
) -> Option<(u32, u32)> {
    let writes = access & WRITE_ACCESS != 0
        || matches!(disposition, CREATE_NEW | CREATE_ALWAYS | TRUNCATE_EXISTING)
        || flags & FILE_FLAG_DELETE_ON_CLOSE != 0;
    let adjusts = disposition == OPEN_ALWAYS || access & MAXIMUM_ALLOWED != 0;
    if !(writes || adjusts) || !read_only_wide(host) {
        return Some((access, disposition));
    }
    if writes {
        refused(call);
        set_last_error(ERROR_ACCESS_DENIED);
        return None;
    }
    let access = if access & MAXIMUM_ALLOWED != 0 {
        (access & !MAXIMUM_ALLOWED) | GENERIC_READ
    } else {
        access
    };
    let disposition = if disposition == OPEN_ALWAYS {
        OPEN_EXISTING
    } else {
        disposition
    };
    Some((access, disposition))
}

/// Refuses a path-changing call (delete, rename, attributes) on a
/// protected path: true when refused, with the last error set.
fn refuse_change(call: &str, host: *const u16) -> bool {
    if read_only_wide(host) {
        refused(call);
        set_last_error(ERROR_ACCESS_DENIED);
        return true;
    }
    false
}

/// Same for the C runtime: true when refused, with errno set.
fn refuse_crt(call: &str, protected: bool) -> bool {
    if protected {
        refused(call);
        set_errno(EACCES);
    }
    protected
}

/// Whether a CRT mode string (`"rb"`, `"w+"`, `"a, ccs=UTF-8"`) writes.
fn mode_writes(mode: impl Iterator<Item = u16>) -> bool {
    mode.take_while(|&c| c != 0 && c != u16::from(b','))
        .any(|c| [b'w', b'a', b'+'].iter().any(|&m| c == u16::from(m)))
}

fn mode_writes_wide(mode: *const u16) -> bool {
    if mode.is_null() {
        return false;
    }
    // SAFETY: a NUL-terminated mode string; reading stops at its NUL.
    mode_writes(
        (0..)
            .map(|i| unsafe { *mode.add(i) })
            .take_while(|&c| c != 0),
    )
}

fn mode_writes_narrow(mode: *const c_char) -> bool {
    !mode.is_null() && mode_writes(widen(mode).into_iter())
}

const O_WRONLY: i32 = 0x1;
const O_RDWR: i32 = 0x2;
const O_APPEND: i32 = 0x8;
const O_CREAT: i32 = 0x100;
const O_TRUNC: i32 = 0x200;
const O_EXCL: i32 = 0x400;

/// `_open` flags made safe for a protected path, or `None` (refused, errno
/// set). `O_CREAT` alone opens only what exists.
fn guard_open_flags(call: &str, protected: impl FnOnce() -> bool, flags: i32) -> Option<i32> {
    let writes = flags & (O_WRONLY | O_RDWR | O_APPEND | O_TRUNC | O_EXCL) != 0;
    if !(writes || flags & O_CREAT != 0) || !protected() {
        return Some(flags);
    }
    if writes {
        refused(call);
        set_errno(EACCES);
        return None;
    }
    Some(flags & !O_CREAT)
}

/// `ios_base::openmode` bits that write: out, app, trunc.
const IOS_WRITES: i32 = 0x2 | 0x8 | 0x10;

// kernel32 and its file API sets.

pub extern "system" fn CreateFileW(
    path: *const u16,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: Handle,
) -> Handle {
    let p = wide(path);
    let Some((access, disposition)) =
        guard_open("CreateFileW", p.ptr(), access, disposition, flags)
    else {
        return INVALID_HANDLE_VALUE;
    };
    host!("kernel32.dll", "CreateFileW": extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
        p.ptr(),
        access,
        share,
        security,
        disposition,
        flags,
        template,
    )
}

pub extern "system" fn CreateFileA(
    path: *const c_char,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: Handle,
) -> Handle {
    match narrow(path) {
        Some(p) => {
            let Some((access, disposition)) =
                guard_open("CreateFileA", p.as_ptr(), access, disposition, flags)
            else {
                return INVALID_HANDLE_VALUE;
            };
            host!("kernel32.dll", "CreateFileW": extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
                p.as_ptr(),
                access,
                share,
                security,
                disposition,
                flags,
                template,
            )
        }
        None => {
            if !path.is_null() && read_only_narrow(path) {
                // The wide twin applies the same rules to the same path.
                let wide: Vec<u16> = unsafe { CStr::from_ptr(path) }
                    .to_string_lossy()
                    .encode_utf16()
                    .chain([0])
                    .collect();
                return CreateFileW(
                    wide.as_ptr(),
                    access,
                    share,
                    security,
                    disposition,
                    flags,
                    template,
                );
            }
            host!("kernel32.dll", "CreateFileA": extern "system" fn(*const c_char, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
                path,
                access,
                share,
                security,
                disposition,
                flags,
                template,
            )
        }
    }
}

/// Used by Bink.
pub extern "system" fn CreateFile2(
    path: *const u16,
    access: u32,
    share: u32,
    disposition: u32,
    params: *const c_void,
) -> Handle {
    let p = wide(path);
    // CREATEFILE2_EXTENDED_PARAMETERS: dwSize, dwFileAttributes, dwFileFlags.
    let flags = if params.is_null() {
        0
    } else {
        // SAFETY: the title passes a whole parameter block or none.
        unsafe { params.cast::<u32>().add(2).read_unaligned() }
    };
    let Some((access, disposition)) =
        guard_open("CreateFile2", p.ptr(), access, disposition, flags)
    else {
        return INVALID_HANDLE_VALUE;
    };
    host!("kernel32.dll", "CreateFile2": extern "system" fn(*const u16, u32, u32, u32, *const c_void) -> Handle)(
        p.ptr(),
        access,
        share,
        disposition,
        params,
    )
}

pub extern "system" fn DeleteFileA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => DeleteFileW(p.as_ptr()),
        None => {
            if read_only_narrow(path) {
                refused("DeleteFileA");
                set_last_error(ERROR_ACCESS_DENIED);
                return 0;
            }
            host!("kernel32.dll", "DeleteFileA": extern "system" fn(*const c_char) -> i32)(path)
        }
    }
}

pub extern "system" fn DeleteFileW(path: *const u16) -> i32 {
    let p = wide(path);
    if refuse_change("DeleteFileW", p.ptr()) {
        return 0;
    }
    host!("kernel32.dll", "DeleteFileW": extern "system" fn(*const u16) -> i32)(p.ptr())
}

pub extern "system" fn RemoveDirectoryW(path: *const u16) -> i32 {
    let p = wide(path);
    if refuse_change("RemoveDirectoryW", p.ptr()) {
        return 0;
    }
    host!("kernel32.dll", "RemoveDirectoryW": extern "system" fn(*const u16) -> i32)(p.ptr())
}

pub extern "system" fn RemoveDirectoryA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => RemoveDirectoryW(p.as_ptr()),
        None => {
            if read_only_narrow(path) {
                refused("RemoveDirectoryA");
                set_last_error(ERROR_ACCESS_DENIED);
                return 0;
            }
            host!("kernel32.dll", "RemoveDirectoryA": extern "system" fn(*const c_char) -> i32)(
                path,
            )
        }
    }
}

pub extern "system" fn SetFileAttributesA(path: *const c_char, attributes: u32) -> i32 {
    match narrow(path) {
        Some(p) => SetFileAttributesW(p.as_ptr(), attributes),
        None => {
            if read_only_narrow(path) {
                refused("SetFileAttributesA");
                set_last_error(ERROR_ACCESS_DENIED);
                return 0;
            }
            host!("kernel32.dll", "SetFileAttributesA": extern "system" fn(*const c_char, u32) -> i32)(
                path, attributes,
            )
        }
    }
}

pub extern "system" fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32 {
    let p = wide(path);
    if refuse_change("SetFileAttributesW", p.ptr()) {
        return 0;
    }
    host!("kernel32.dll", "SetFileAttributesW": extern "system" fn(*const u16, u32) -> i32)(
        p.ptr(),
        attributes,
    )
}

pub extern "system" fn GetFileAttributesA(path: *const c_char) -> u32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "GetFileAttributesW": extern "system" fn(*const u16) -> u32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "GetFileAttributesA": extern "system" fn(*const c_char) -> u32)(
                path,
            )
        }
    }
}

pub extern "system" fn CreateDirectoryW(path: *const u16, security: *const c_void) -> i32 {
    let p = wide(path);
    if read_only_wide(p.ptr()) {
        // What exists already answers as it would anywhere; nothing new.
        const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
        let exists = host!("kernel32.dll", "GetFileAttributesW": extern "system" fn(*const u16) -> u32)(
            p.ptr(),
        ) != INVALID_FILE_ATTRIBUTES;
        if !exists {
            refused("CreateDirectoryW");
        }
        set_last_error(if exists {
            ERROR_ALREADY_EXISTS
        } else {
            ERROR_ACCESS_DENIED
        });
        return 0;
    }
    host!("kernel32.dll", "CreateDirectoryW": extern "system" fn(*const u16, *const c_void) -> i32)(
        p.ptr(),
        security,
    )
}

pub extern "system" fn GetFileAttributesExW(
    path: *const u16,
    level: i32,
    info: *mut c_void,
) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "GetFileAttributesExW": extern "system" fn(*const u16, i32, *mut c_void) -> i32)(
        p.ptr(),
        level,
        info,
    )
}

pub extern "system" fn GetFileAttributesW(path: *const u16) -> u32 {
    let p = wide(path);
    host!("kernel32.dll", "GetFileAttributesW": extern "system" fn(*const u16) -> u32)(p.ptr())
}

pub extern "system" fn GetDiskFreeSpaceExW(
    path: *const u16,
    available: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "GetDiskFreeSpaceExW": extern "system" fn(*const u16, *mut u64, *mut u64, *mut u64) -> i32)(
        p.ptr(),
        available,
        total,
        free,
    )
}

/// Wants a volume root: a virtual drive answers for the host volume its
/// folder is on.
pub extern "system" fn GetDiskFreeSpaceW(
    root: *const u16,
    sectors_per_cluster: *mut u32,
    bytes_per_sector: *mut u32,
    free_clusters: *mut u32,
    clusters: *mut u32,
) -> i32 {
    let mut p = wide(root);
    if let Some(host) = &mut p.1
        && let [letter, colon, sep, ..] = host[..]
        && colon == u16::from(b':')
        && SEPARATORS.contains(&sep)
    {
        *host = vec![letter, colon, sep, 0];
    }
    host!("kernel32.dll", "GetDiskFreeSpaceW": extern "system" fn(*const u16, *mut u32, *mut u32, *mut u32, *mut u32) -> i32)(
        p.ptr(),
        sectors_per_cluster,
        bytes_per_sector,
        free_clusters,
        clusters,
    )
}

pub extern "system" fn FindFirstFileW(path: *const u16, data: *mut c_void) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "FindFirstFileW": extern "system" fn(*const u16, *mut c_void) -> Handle)(
        p.ptr(),
        data,
    )
}

pub extern "system" fn FindFirstFileExW(
    path: *const u16,
    level: i32,
    data: *mut c_void,
    search: i32,
    filter: *mut c_void,
    flags: u32,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "FindFirstFileExW": extern "system" fn(*const u16, i32, *mut c_void, i32, *mut c_void, u32) -> Handle)(
        p.ptr(),
        level,
        data,
        search,
        filter,
        flags,
    )
}

pub extern "system" fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32 {
    let (from, to) = (wide(from), wide(to));
    if refuse_change("MoveFileExW", from.ptr()) || refuse_change("MoveFileExW", to.ptr()) {
        return 0;
    }
    host!("kernel32.dll", "MoveFileExW": extern "system" fn(*const u16, *const u16, u32) -> i32)(
        from.ptr(),
        to.ptr(),
        flags,
    )
}

pub extern "system" fn MoveFileW(from: *const u16, to: *const u16) -> i32 {
    const MOVEFILE_COPY_ALLOWED: u32 = 0x2;
    MoveFileExW(from, to, MOVEFILE_COPY_ALLOWED)
}

pub extern "system" fn CopyFileW(from: *const u16, to: *const u16, fail_if_exists: i32) -> i32 {
    let (from, to) = (wide(from), wide(to));
    if refuse_change("CopyFileW", to.ptr()) {
        return 0;
    }
    host!("kernel32.dll", "CopyFileW": extern "system" fn(*const u16, *const u16, i32) -> i32)(
        from.ptr(),
        to.ptr(),
        fail_if_exists,
    )
}

pub extern "system" fn ReplaceFileW(
    replaced: *const u16,
    replacement: *const u16,
    backup: *const u16,
    flags: u32,
    exclude: *mut c_void,
    reserved: *mut c_void,
) -> i32 {
    let (replaced, replacement, backup) = (wide(replaced), wide(replacement), wide(backup));
    if refuse_change("ReplaceFileW", replaced.ptr())
        || refuse_change("ReplaceFileW", replacement.ptr())
        || refuse_change("ReplaceFileW", backup.ptr())
    {
        return 0;
    }
    host!("kernel32.dll", "ReplaceFileW": extern "system" fn(*const u16, *const u16, *const u16, u32, *mut c_void, *mut c_void) -> i32)(
        replaced.ptr(),
        replacement.ptr(),
        backup.ptr(),
        flags,
        exclude,
        reserved,
    )
}

pub extern "system" fn SetCurrentDirectoryA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "SetCurrentDirectoryW": extern "system" fn(*const u16) -> i32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "SetCurrentDirectoryA": extern "system" fn(*const c_char) -> i32)(
                path,
            )
        }
    }
}

// The C runtime (ucrtbase, which the title's CRT imports bind to).

pub extern "C" fn _wfopen(path: *const u16, mode: *const u16) -> File {
    let p = wide(path);
    if refuse_crt("_wfopen", mode_writes_wide(mode) && read_only_wide(p.ptr())) {
        return std::ptr::null_mut();
    }
    host!("ucrtbase.dll", "_wfopen": extern "C" fn(*const u16, *const u16) -> File)(p.ptr(), mode)
}

pub extern "C" fn _wfopen_s(out: *mut File, path: *const u16, mode: *const u16) -> i32 {
    let p = wide(path);
    if refuse_crt(
        "_wfopen_s",
        mode_writes_wide(mode) && read_only_wide(p.ptr()),
    ) {
        if !out.is_null() {
            // SAFETY: the title's out pointer.
            unsafe { *out = std::ptr::null_mut() };
        }
        return EACCES;
    }
    host!("ucrtbase.dll", "_wfopen_s": extern "C" fn(*mut File, *const u16, *const u16) -> i32)(
        out,
        p.ptr(),
        mode,
    )
}

pub extern "C" fn _wfsopen(path: *const u16, mode: *const u16, share: i32) -> File {
    let p = wide(path);
    if refuse_crt(
        "_wfsopen",
        mode_writes_wide(mode) && read_only_wide(p.ptr()),
    ) {
        return std::ptr::null_mut();
    }
    host!("ucrtbase.dll", "_wfsopen": extern "C" fn(*const u16, *const u16, i32) -> File)(
        p.ptr(),
        mode,
        share,
    )
}

// Kernel reads into title memory. The GPU side leaves blocks of title GPU
// buffers read-only to see the title's next write, and the kernel's writes
// fail there instead of faulting into its handler; the title's
// synchronous reads (title+0x313fc0) read straight into 4 KiB-aligned
// destinations and retry a failed `ReadFile` forever. So the destination
// is opened first. A block guarded again before the kernel writes it (the
// guard over all the views of a page protects other views from other
// threads) fails that `ReadFile`, and the title's retry opens it again;
// `fread` has no such retry and reads beside instead (see there).

pub extern "system" fn ReadFile(
    file: *mut c_void,
    buffer: *mut u8,
    len: u32,
    read: *mut u32,
    overlapped: *mut c_void,
) -> i32 {
    victory_gpu::kernel_will_write(buffer as usize, len as usize);
    host!("kernel32.dll", "ReadFile": extern "system" fn(*mut c_void, *mut u8, u32, *mut u32, *mut c_void) -> i32)(
        file, buffer, len, read, overlapped,
    )
}

/// The host CRT reads large requests straight into the caller's buffer
/// through the kernel, whose write fails on a guarded block instead of
/// faulting into the guard. So the bytes are read beside, a chunk at a time
/// (a caller may ask for far more than the file holds), and copied in from
/// user mode, where a guarded block faults into the guard like the title's
/// own writes. Returns the items read, as the CRT does; the bytes of a
/// partial last item are copied too, as the CRT leaves them.
pub extern "C" fn fread(buffer: *mut u8, size: usize, count: usize, stream: File) -> usize {
    let read = host!("ucrtbase.dll", "fread": extern "C" fn(*mut u8, usize, usize, File) -> usize);
    let total = size.saturating_mul(count);
    if buffer.is_null() || total == 0 {
        // The CRT's own checks and answer (EINVAL, or nothing to read).
        return read(buffer, size, count, stream);
    }
    const CHUNK: usize = 1 << 20;
    let mut beside = vec![0u8; total.min(CHUNK)];
    let mut done = 0;
    while done < total {
        let want = (total - done).min(CHUNK);
        let n = read(beside.as_mut_ptr(), 1, want, stream);
        if n > 0 {
            victory_gpu::kernel_will_write(buffer as usize + done, n);
            // SAFETY: the caller's buffer holds `size * count` bytes, and
            // `done + n` is at most that.
            unsafe { std::ptr::copy_nonoverlapping(beside.as_ptr(), buffer.add(done), n) };
            done += n;
        }
        if n < want {
            break;
        }
    }
    done / size
}

pub extern "C" fn fopen(path: *const c_char, mode: *const c_char) -> File {
    match narrow(path) {
        Some(p) => {
            if refuse_crt("fopen", mode_writes_narrow(mode) && read_only_wide(p.as_ptr())) {
                return std::ptr::null_mut();
            }
            host!("ucrtbase.dll", "_wfopen": extern "C" fn(*const u16, *const u16) -> File)(
                p.as_ptr(),
                widen(mode).as_ptr(),
            )
        }
        None => {
            if refuse_crt("fopen", mode_writes_narrow(mode) && read_only_narrow(path)) {
                return std::ptr::null_mut();
            }
            host!("ucrtbase.dll", "fopen": extern "C" fn(*const c_char, *const c_char) -> File)(
                path, mode,
            )
        }
    }
}

pub extern "C" fn fopen_s(out: *mut File, path: *const c_char, mode: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            if refuse_crt("fopen_s", mode_writes_narrow(mode) && read_only_wide(p.as_ptr())) {
                if !out.is_null() {
                    // SAFETY: the title's out pointer.
                    unsafe { *out = std::ptr::null_mut() };
                }
                return EACCES;
            }
            host!("ucrtbase.dll", "_wfopen_s": extern "C" fn(*mut File, *const u16, *const u16) -> i32)(
                out,
                p.as_ptr(),
                widen(mode).as_ptr(),
            )
        }
        None => {
            if refuse_crt(
                "fopen_s",
                mode_writes_narrow(mode) && read_only_narrow(path),
            ) {
                if !out.is_null() {
                    // SAFETY: the title's out pointer.
                    unsafe { *out = std::ptr::null_mut() };
                }
                return EACCES;
            }
            host!("ucrtbase.dll", "fopen_s": extern "C" fn(*mut File, *const c_char, *const c_char) -> i32)(
                out, path, mode,
            )
        }
    }
}

/// `_open(path, flags[, mode])` is variadic; on x64 the optional integer
/// travels in the same register either way, so it is passed on as is.
pub extern "C" fn _open(path: *const c_char, flags: i32, mode: i32) -> i32 {
    match narrow(path) {
        Some(p) => _wopen(p.as_ptr(), flags, mode),
        None => {
            let Some(flags) = guard_open_flags("_open", || read_only_narrow(path), flags) else {
                return -1;
            };
            host!("ucrtbase.dll", "_open": extern "C" fn(*const c_char, i32, i32) -> i32)(
                path, flags, mode,
            )
        }
    }
}

/// See [`_open`]; used by libHttpClient.
pub extern "C" fn _wopen(path: *const u16, flags: i32, mode: i32) -> i32 {
    let p = wide(path);
    let Some(flags) = guard_open_flags("_wopen", || read_only_wide(p.ptr()), flags) else {
        return -1;
    };
    host!("ucrtbase.dll", "_wopen": extern "C" fn(*const u16, i32, i32) -> i32)(
        p.ptr(),
        flags,
        mode,
    )
}

pub extern "C" fn _wstat64(path: *const u16, stat: *mut c_void) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wstat64": extern "C" fn(*const u16, *mut c_void) -> i32)(p.ptr(), stat)
}

pub extern "C" fn _stat64i32(path: *const c_char, stat: *mut c_void) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("ucrtbase.dll", "_wstat64i32": extern "C" fn(*const u16, *mut c_void) -> i32)(
                p.as_ptr(),
                stat,
            )
        }
        None => {
            host!("ucrtbase.dll", "_stat64i32": extern "C" fn(*const c_char, *mut c_void) -> i32)(
                path, stat,
            )
        }
    }
}

pub extern "C" fn remove(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => _wremove(p.as_ptr()),
        None => {
            if refuse_crt("remove", read_only_narrow(path)) {
                return -1;
            }
            host!("ucrtbase.dll", "remove": extern "C" fn(*const c_char) -> i32)(path)
        }
    }
}

pub extern "C" fn _wremove(path: *const u16) -> i32 {
    let p = wide(path);
    if refuse_crt("_wremove", read_only_wide(p.ptr())) {
        return -1;
    }
    host!("ucrtbase.dll", "_wremove": extern "C" fn(*const u16) -> i32)(p.ptr())
}

pub extern "C" fn _wrename(from: *const u16, to: *const u16) -> i32 {
    let (from, to) = (wide(from), wide(to));
    if refuse_crt(
        "_wrename",
        read_only_wide(from.ptr()) || read_only_wide(to.ptr()),
    ) {
        return -1;
    }
    host!("ucrtbase.dll", "_wrename": extern "C" fn(*const u16, *const u16) -> i32)(
        from.ptr(),
        to.ptr(),
    )
}

pub extern "C" fn rename(from: *const c_char, to: *const c_char) -> i32 {
    let (wf, wt) = (narrow(from), narrow(to));
    if wf.is_some() || wt.is_some() {
        // Either side on a virtual drive: both go wide, the other as the host
        // decodes it.
        let decode = |p: *const c_char| -> Vec<u16> {
            // SAFETY: a NUL-terminated path.
            unsafe { CStr::from_ptr(p) }
                .to_string_lossy()
                .encode_utf16()
                .chain([0])
                .collect()
        };
        let wf = wf.unwrap_or_else(|| decode(from));
        let wt = wt.unwrap_or_else(|| decode(to));
        return _wrename(wf.as_ptr(), wt.as_ptr());
    }
    if refuse_crt("rename", read_only_narrow(from) || read_only_narrow(to)) {
        return -1;
    }
    host!("ucrtbase.dll", "rename": extern "C" fn(*const c_char, *const c_char) -> i32)(from, to)
}

pub extern "C" fn _wchmod(path: *const u16, mode: i32) -> i32 {
    let p = wide(path);
    if refuse_crt("_wchmod", read_only_wide(p.ptr())) {
        return -1;
    }
    host!("ucrtbase.dll", "_wchmod": extern "C" fn(*const u16, i32) -> i32)(p.ptr(), mode)
}

pub extern "C" fn _wutime64(path: *const u16, times: *mut c_void) -> i32 {
    let p = wide(path);
    if refuse_crt("_wutime64", read_only_wide(p.ptr())) {
        return -1;
    }
    host!("ucrtbase.dll", "_wutime64": extern "C" fn(*const u16, *mut c_void) -> i32)(
        p.ptr(),
        times,
    )
}

// The C++ library: `std::basic_filebuf::open` goes through `_Fiopen`.

const FIOPEN: &str = "?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z";
const FIOPEN_WIDE: &str = "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z";

/// `_Fiopen(const wchar_t* path, ios_base::openmode, int protection)`.
extern "C" fn fiopen_wide(path: *const u16, mode: i32, protection: i32) -> File {
    let p = wide(path);
    if refuse_crt("_Fiopen", mode & IOS_WRITES != 0 && read_only_wide(p.ptr())) {
        return std::ptr::null_mut();
    }
    host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z": extern "C" fn(*const u16, i32, i32) -> File)(
        p.ptr(),
        mode,
        protection,
    )
}

/// `_Fiopen(const char* path, ios_base::openmode, int protection)`.
extern "C" fn fiopen(path: *const c_char, mode: i32, protection: i32) -> File {
    match narrow(path) {
        Some(p) => {
            if refuse_crt("_Fiopen", mode & IOS_WRITES != 0 && read_only_wide(p.as_ptr())) {
                return std::ptr::null_mut();
            }
            host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z": extern "C" fn(*const u16, i32, i32) -> File)(
                p.as_ptr(),
                mode,
                protection,
            )
        }
        None => {
            if refuse_crt("_Fiopen", mode & IOS_WRITES != 0 && read_only_narrow(path)) {
                return std::ptr::null_mut();
            }
            host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z": extern "C" fn(*const c_char, i32, i32) -> File)(
                path, mode, protection,
            )
        }
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "files",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-file-*",
            "api-ms-win-core-processenvironment-*",
            "ucrtbase.dll",
            "api-ms-win-crt-stdio-*",
            "api-ms-win-crt-filesystem-*",
            "api-ms-win-crt-time-*",
            "msvcp140.dll",
        ],
        exports: vec![
            hle_export!(ReadFile),
            hle_export!(fread),
            hle_export!(CreateFileW),
            hle_export!(CreateFileA),
            hle_export!(CreateFile2),
            hle_export!(DeleteFileA),
            hle_export!(DeleteFileW),
            hle_export!(RemoveDirectoryW),
            hle_export!(SetFileAttributesW),
            hle_export!(GetFileAttributesA),
            hle_export!(CreateDirectoryW),
            hle_export!(GetFileAttributesW),
            hle_export!(GetFileAttributesExW),
            hle_export!(GetDiskFreeSpaceExW),
            hle_export!(GetDiskFreeSpaceW),
            hle_export!(FindFirstFileW),
            hle_export!(FindFirstFileExW),
            hle_export!(MoveFileExW),
            hle_export!(MoveFileW),
            hle_export!(CopyFileW),
            hle_export!(ReplaceFileW),
            hle_export!(RemoveDirectoryA),
            hle_export!(SetFileAttributesA),
            hle_export!(SetCurrentDirectoryA),
            hle_export!(_wfopen),
            hle_export!(_wfopen_s),
            hle_export!(_wfsopen),
            hle_export!(fopen),
            hle_export!(fopen_s),
            hle_export!(_open),
            hle_export!(_wopen),
            hle_export!(_wstat64),
            hle_export!(_stat64i32),
            hle_export!(remove),
            hle_export!(_wremove),
            hle_export!(rename),
            hle_export!(_wrename),
            hle_export!(_wchmod),
            hle_export!(_wutime64),
            HleExport {
                name: FIOPEN,
                address: fiopen as *const () as usize,
            },
            HleExport {
                name: FIOPEN_WIDE,
                address: fiopen_wide as *const () as usize,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn kernel_reads_reach_the_buffer() {
        use std::os::windows::io::AsRawHandle;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, b"0123456789").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let (mut out, mut n) = ([0u8; 10], 0);
        let handle = file.as_raw_handle().cast();
        assert_ne!(
            ReadFile(handle, out.as_mut_ptr(), 10, &mut n, std::ptr::null_mut()),
            0
        );
        assert_eq!((&out, n), (b"0123456789", 10));
    }

    #[test]
    fn fread_reads_beside_in_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        // Over two chunks, and not a whole number of 3-byte items.
        let bytes: Vec<u8> = (0..(5 << 19) + 1).map(|i| (i * 7 + i / 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let fclose = host!("ucrtbase.dll", "fclose": extern "C" fn(File) -> i32);
        let open = || {
            let f = _wfopen(w(&path.to_string_lossy()).as_ptr(), w("rb").as_ptr());
            assert!(!f.is_null());
            f
        };

        // Asked for far more than the file holds: the whole file, in items.
        let f = open();
        let mut out = vec![0xaau8; bytes.len() + 64];
        let items = fread(out.as_mut_ptr(), 3, out.len() / 3, f);
        assert_eq!(items, bytes.len() / 3);
        assert_eq!(&out[..bytes.len()], &bytes[..], "the partial last item's bytes too");
        assert!(out[bytes.len()..].iter().all(|&b| b == 0xaa), "nothing past what was read");
        fclose(f);

        // Exact reads continue where the last one stopped.
        let f = open();
        let mut a = vec![0u8; 1000];
        let mut b = vec![0u8; (1 << 20) + 5];
        assert_eq!(fread(a.as_mut_ptr(), 1000, 1, f), 1);
        assert_eq!(fread(b.as_mut_ptr(), 1, b.len(), f), b.len());
        assert_eq!(a, bytes[..1000]);
        assert_eq!(b, bytes[1000..1000 + b.len()]);
        fclose(f);
    }

    fn text(v: &[u16]) -> String {
        String::from_utf16_lossy(&v[..v.len() - 1])
    }

    #[test]
    fn rewrites_only_paths_on_mounted_drives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        let letter = mount(dir.path()).unwrap();
        assert_eq!(
            mount(dir.path()),
            Some(letter),
            "mounting twice reuses the letter"
        );
        let tr = |s: String| translate(&w(&s)[..s.encode_utf16().count()]).map(|v| text(&v));
        assert_eq!(
            tr(format!("{letter}:/a/b.txt")),
            Some(format!("{root}/a/b.txt"))
        );
        let lower = letter.to_ascii_lowercase();
        assert_eq!(tr(format!("{lower}:\\a")), Some(format!("{root}\\a")));
        assert_eq!(tr(format!("{letter}:")), Some(format!("{root}\\")));
        assert_eq!(
            tr(format!(r"\\?\{letter}:/a/b")),
            Some(format!(r"\\?\{root}\a\b"))
        );
        assert_eq!(tr(r"C:\Windows".into()), None);
        assert_eq!(tr("relative/path".into()), None);
        assert_eq!(tr(String::new()), None);

        // Fixed letters, as the console's package drive; a later mount of
        // the same letter replaces the earlier one.
        let package = tempfile::tempdir().unwrap();
        mount_at('g', dir.path());
        mount_at('G', package.path());
        assert_eq!(
            tr(r"G:\appdata0.rpf".into()),
            Some(format!(r"{}\appdata0.rpf", package.path().display()))
        );
        assert_ne!(mount(dir.path()), Some('G'), "free letters skip fixed ones");

        // Long ones take the `\\?\` form, cleaned up as the host would.
        let long = "d".repeat(250);
        assert_eq!(
            tr(format!(r"G:/a/./b/../{long}//f")),
            Some(format!(r"\\?\{}\a\{long}\f", package.path().display()))
        );
        assert_eq!(
            verbatim(&w(r"\\server\share\")[..15], &w("/x/../../y")[..10]),
            w(r"\\?\UNC\server\share\y")[..22].to_vec()
        );
    }

    #[test]
    fn long_title_paths_work_on_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let letter = mount(dir.path()).unwrap();
        let deep = format!("{letter}:/{}/{}", "a".repeat(200), "b".repeat(100));
        let null = std::ptr::null_mut();
        let first = w(&deep[..203]);
        assert_ne!(CreateDirectoryW(first.as_ptr(), null), 0);
        assert_ne!(CreateDirectoryW(w(&deep).as_ptr(), null), 0);
        let file = format!("{deep}/../{}/f.txt", "b".repeat(100));
        let f = _wfopen(w(&file).as_ptr(), w("wb").as_ptr());
        assert!(!f.is_null(), "{file}");
        host!("ucrtbase.dll", "fclose": extern "C" fn(File) -> i32)(f);
        assert_ne!(DeleteFileW(w(&format!("{deep}/f.txt")).as_ptr()), 0);
        assert_ne!(RemoveDirectoryW(w(&deep).as_ptr()), 0);
    }

    /// A protected folder (the user's game copy) refuses every change,
    /// through a virtual drive or its host path, and still reads.
    #[test]
    fn protected_folders_refuse_writes_and_still_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pkg.rpf"), b"RPF9").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let before = std::fs::metadata(dir.path().join("pkg.rpf")).unwrap();
        let letter = mount(dir.path()).unwrap();
        protect(dir.path());
        let null = std::ptr::null_mut();
        let close = host!("kernel32.dll", "CloseHandle": extern "system" fn(Handle) -> i32);
        let fclose = host!("ucrtbase.dll", "fclose": extern "C" fn(File) -> i32);
        const GENERIC_WRITE: u32 = 0x4000_0000;

        let on_drive = w(&format!(r"{letter}:\pkg.rpf"));
        let on_host = w(&dir.path().join("pkg.rpf").display().to_string());
        let escaped = w(&format!(r"{}\sub\..\pkg.rpf", dir.path().display()));
        for path in [&on_drive, &on_host, &escaped] {
            for disposition in [CREATE_ALWAYS, TRUNCATE_EXISTING, OPEN_EXISTING] {
                let h = CreateFileW(path.as_ptr(), GENERIC_WRITE, 0, null, disposition, 0, null);
                assert_eq!(h, INVALID_HANDLE_VALUE);
            }
            assert_eq!(DeleteFileW(path.as_ptr()), 0);
            assert_eq!(SetFileAttributesW(path.as_ptr(), 1), 0);
            assert!(_wfopen(path.as_ptr(), w("r+b").as_ptr()).is_null());
            assert!(_wfopen(path.as_ptr(), w("ab").as_ptr()).is_null());
            assert_eq!(_wopen(path.as_ptr(), O_RDWR, 0), -1);
            assert_eq!(_wchmod(path.as_ptr(), 0), -1);
            assert_eq!(_wutime64(path.as_ptr(), null), -1);
            assert!(fiopen_wide(path.as_ptr(), 0x2, 0x40).is_null());
            assert_eq!(_wrename(path.as_ptr(), w(r"C:\elsewhere.rpf").as_ptr()), -1);
        }
        let new = w(&format!(r"{letter}:\new.bin"));
        assert_eq!(
            CreateFileW(new.as_ptr(), GENERIC_READ, 0, null, OPEN_ALWAYS, 0, null),
            INVALID_HANDLE_VALUE
        );
        assert_eq!(_wopen(new.as_ptr(), O_CREAT, 0x80), -1);
        assert_eq!(
            CreateDirectoryW(w(&format!(r"{letter}:\made")).as_ptr(), null),
            0
        );
        assert_eq!(
            MoveFileExW(
                on_drive.as_ptr(),
                w(&format!(r"{letter}:\moved")).as_ptr(),
                0
            ),
            0
        );
        let fname = format!("{letter}:/pkg.rpf\0");
        assert!(fopen(fname.as_ptr().cast(), c"wb".as_ptr()).is_null());
        assert_eq!(remove(fname.as_ptr().cast()), -1);

        // Reads still work.
        let h = CreateFileW(
            on_drive.as_ptr(),
            GENERIC_READ,
            1,
            null,
            OPEN_ALWAYS,
            0,
            null,
        );
        assert_ne!(h, INVALID_HANDLE_VALUE);
        close(h);
        let f = _wfopen(on_host.as_ptr(), w("rb").as_ptr());
        assert!(!f.is_null());
        fclose(f);

        // Nothing changed, nothing appeared.
        let after = std::fs::metadata(dir.path().join("pkg.rpf")).unwrap();
        assert_eq!(std::fs::read(dir.path().join("pkg.rpf")).unwrap(), b"RPF9");
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert!(!after.permissions().readonly());
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["pkg.rpf", "sub"]);
    }

    #[test]
    fn title_file_calls_land_in_the_host_folder() {
        let dir = tempfile::tempdir().unwrap();
        let letter = mount(dir.path()).unwrap();
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const CREATE_ALWAYS: u32 = 2;
        let close = host!("kernel32.dll", "CloseHandle": extern "system" fn(Handle) -> i32);
        let fclose = host!("ucrtbase.dll", "fclose": extern "C" fn(File) -> i32);

        let path = w(&format!(r"{letter}:\created.bin"));
        let null = std::ptr::null_mut();
        let h = CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            null,
            CREATE_ALWAYS,
            0,
            null,
        );
        assert_ne!(h as isize, -1);
        close(h);
        assert!(dir.path().join("created.bin").is_file());

        let path = format!("{letter}:/opened.txt\0");
        let f = fopen(path.as_ptr().cast(), c"wb".as_ptr());
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("opened.txt").is_file());

        let path = format!("{letter}:/stream.txt\0");
        const OUT: i32 = 2;
        let f = fiopen(path.as_ptr().cast(), OUT, 0x40);
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("stream.txt").is_file());

        assert_eq!(remove(path.as_ptr().cast()), 0);
        assert!(!dir.path().join("stream.txt").exists());
        let f = fiopen_wide(w(&format!("{letter}:/wide.txt")).as_ptr(), OUT, 0x40);
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("wide.txt").is_file());

        const OPEN_EXISTING: u32 = 3;
        let path = w(&format!(r"{letter}:\created.bin"));
        let h = CreateFile2(path.as_ptr(), GENERIC_WRITE, 0, OPEN_EXISTING, null);
        assert_ne!(h as isize, -1);
        close(h);
        const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
        assert_ne!(GetFileAttributesW(path.as_ptr()), INVALID_FILE_ATTRIBUTES);

        const O_WRONLY: i32 = 1;
        const O_CREAT: i32 = 0x100;
        const S_IWRITE: i32 = 0x80;
        let path = format!("{letter}:/low.bin\0");
        let fd = _open(path.as_ptr().cast(), O_WRONLY | O_CREAT, S_IWRITE);
        assert!(fd >= 0);
        host!("ucrtbase.dll", "_close": extern "C" fn(i32) -> i32)(fd);
        assert!(dir.path().join("low.bin").is_file());

        // Other paths pass through untouched.
        let host_path = format!("{}\0", dir.path().join("plain.txt").display());
        let f = fopen(host_path.as_ptr().cast(), c"wb".as_ptr());
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("plain.txt").is_file());
    }

    /// A narrow path is rewritten once, as its wide twin is. The inner
    /// drive's folder lies on the outer virtual drive, as the title folder
    /// does when it sits on a host `G:` drive and is mounted as `G:`. The
    /// outer letter is no host drive, so the host finds nothing there; a
    /// narrow call that reaches the outer folder rewrote the path twice.
    #[test]
    fn narrow_paths_are_rewritten_once() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("inner");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("present.txt"), b"").unwrap();
        let outer = mount(dir.path()).unwrap();
        let inner = mount(Path::new(&format!(r"{outer}:\inner"))).unwrap();
        let path = |name: &str| format!("{inner}:/{name}\0");

        const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
        let wide = GetFileAttributesW(w(&format!("{inner}:/present.txt")).as_ptr());
        assert_eq!(wide, INVALID_FILE_ATTRIBUTES);
        let narrow = GetFileAttributesA(path("present.txt").as_ptr().cast());
        assert_eq!(narrow, wide, "GetFileAttributesA agrees with its wide twin");

        const GENERIC_WRITE: u32 = 0x4000_0000;
        const CREATE_ALWAYS: u32 = 2;
        let null = std::ptr::null_mut();
        let h = CreateFileA(
            path("a.bin").as_ptr().cast(),
            GENERIC_WRITE,
            0,
            null,
            CREATE_ALWAYS,
            0,
            null,
        );
        assert_eq!(h as isize, -1);
        assert!(fopen(path("b.txt").as_ptr().cast(), c"wb".as_ptr()).is_null());
        let mut f: File = std::ptr::null_mut();
        let error = fopen_s(&mut f, path("c.txt").as_ptr().cast(), c"wb".as_ptr());
        assert_ne!(error, 0);
        const OUT: i32 = 2;
        assert!(fiopen(path("d.txt").as_ptr().cast(), OUT, 0x40).is_null());

        // Nothing was made behind a path rewritten twice.
        let names: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["present.txt"]);
    }
}
