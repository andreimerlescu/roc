//! Small helpers: ids, timestamps, process liveness, file permissions,
//! logging. Everything platform specific lives here.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Returns `n` random bytes from the OS CSPRNG, hex encoded.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if getrandom::fill(&mut buf).is_err() {
        // Practically impossible; fall back to time+pid mixing so ids stay unique.
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            ^ ((std::process::id() as u128) << 64);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed >> ((i % 16) * 8)) as u8 ^ (i as u8).wrapping_mul(31);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A new session id: 12 hex chars (48 bits of randomness).
pub fn new_session_id() -> String {
    random_hex(6)
}

/// RFC 3339 UTC timestamp with second precision, e.g. `2026-10-07T14:03:00Z`.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_rfc3339(secs)
}

/// Formats unix seconds as RFC 3339 UTC.
pub fn format_rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// True when a process with this pid exists.
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: kill with signal 0 performs only permission/existence checks.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// True when a process with this pid exists.
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == 0 {
        return false;
    }
    // SAFETY: plain Win32 calls; the handle is closed before returning.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

/// The machine hostname (best effort).
pub fn hostname() -> String {
    let h = gethostname::gethostname().to_string_lossy().into_owned();
    if h.is_empty() { "unknown".into() } else { h }
}

/// The uid:gid the agent container runs as: yours on Unix (so files you
/// create stay yours); a fixed non-root user on Windows.
pub fn container_user() -> (u32, u32) {
    #[cfg(unix)]
    {
        // SAFETY: getuid/getgid never fail.
        unsafe { (libc::getuid(), libc::getgid()) }
    }
    #[cfg(not(unix))]
    {
        (1000, 1000)
    }
}

/// The user's home directory (`$HOME`; `%USERPROFILE%` first on Windows).
pub fn home_dir() -> Option<PathBuf> {
    let order: &[&str] = if cfg!(windows) {
        &["USERPROFILE", "HOME"]
    } else {
        &["HOME"]
    };
    order
        .iter()
        .filter_map(std::env::var_os)
        .find(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Executable file name candidates for `cmd` (`npx` → `npx.exe`, `npx.cmd`, … on Windows).
fn exe_candidates(cmd: &str) -> Vec<String> {
    if !cfg!(windows) || Path::new(cmd).extension().is_some() {
        return vec![cmd.to_string()];
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    exts.split(';')
        .filter(|e| !e.is_empty())
        .map(|e| format!("{cmd}{}", e.to_ascii_lowercase()))
        .collect()
}

/// Locates an executable on `PATH` (honouring `PATHEXT` on Windows).
pub fn which(cmd: &str) -> Option<PathBuf> {
    if cmd.contains('/') || cmd.contains(std::path::MAIN_SEPARATOR) {
        return exe_candidates(cmd)
            .into_iter()
            .map(PathBuf::from)
            .find(|p| is_executable(p));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| exe_candidates(cmd).into_iter().map(move |c| dir.join(c)))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    let Ok(m) = std::fs::metadata(p) else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.is_file() && m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        m.is_file()
    }
}

/// Canonical path without Windows `\\?\` prefixes (Docker can't use those).
pub fn canonicalize(p: &Path) -> std::io::Result<PathBuf> {
    dunce::canonicalize(p)
}

/// Creates a directory tree; on Unix the leaf is restricted to 0700.
pub fn ensure_private_dir(p: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(p)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_options() -> OpenOptions {
    #[allow(unused_mut)]
    let mut o = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o
}

/// Writes a file (mode 0600 on Unix), truncating.
pub fn write_private_file(p: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut f = private_options().create(true).write(true).truncate(true).open(p)?;
    f.write_all(contents)?;
    f.sync_all()
}

/// FNV-1a 64-bit hash, hex encoded (stable identifiers, not security).
pub fn fnv1a_hex(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Constant-time byte comparison (for bearer tokens).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Truncates text to at most `max` bytes on a char boundary, noting the truncation.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[truncated {} bytes]", &s[..end], s.len() - end)
}

// ---------------------------------------------------------------------------
// File logger. While the agent TUI owns the terminal, roc must never write to
// stdout/stderr, so everything after launch goes to a per-session log file.
// ---------------------------------------------------------------------------

static LOG: Mutex<Option<File>> = Mutex::new(None);

/// Directs `rlog!` output to `path` (append).
pub fn init_log(path: &Path) -> std::io::Result<()> {
    let f = private_options().create(true).append(true).open(path)?;
    *LOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    Ok(())
}

/// Writes one line to the session log (no-op until `init_log`).
pub fn log_line(msg: &str) {
    if let Some(f) = LOG.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = writeln!(f, "{} {}", now_rfc3339(), msg);
    }
}

/// `rlog!("fmt", args..)` — log to the session log file.
#[macro_export]
macro_rules! rlog {
    ($($arg:tt)*) => { $crate::util::log_line(&format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_rfc3339(1_791_374_580), "2026-10-07T12:03:00Z");
    }

    #[test]
    fn random_ids_are_hex_and_unique() {
        let a = new_session_id();
        let b = new_session_id();
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn pid_alive_self_and_bogus() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX));
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a_hex(b""), "cbf29ce484222325");
        assert_eq!(fnv1a_hex(b"a"), "af63dc4c8601ec8c");
        assert_ne!(fnv1a_hex(b"/a/state.json"), fnv1a_hex(b"/b/state.json"));
    }

    #[test]
    fn ct_eq_works() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn truncate_respects_boundaries() {
        assert_eq!(truncate("hello", 10), "hello");
        let t = truncate("héllo", 2);
        assert!(t.starts_with('h'));
        assert!(t.contains("truncated"));
    }

    #[test]
    #[cfg(unix)]
    fn which_finds_sh() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }
}
