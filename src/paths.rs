//! Parsing and validation of the `-read-dir` / `-write-dir` CSV lists.
//!
//! Every mount is mapped 1:1: the path the user typed (after `~` expansion and
//! lexical normalisation) is the path inside the container. The *source* of the
//! bind mount is the canonical host path, so symlinked directories still work.
//!
//! On Windows the container is Linux, so `C:\Users\me\p` appears as
//! `/c/Users/me/p` (the convention Docker Desktop and Git Bash use);
//! [`container_path`] and [`host_path`] convert between the two.

use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Access mode of a bind mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountMode {
    /// Read-only (`readonly` bind).
    Ro,
    /// Read-write.
    Rw,
}

impl fmt::Display for MountMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MountMode::Ro => "ro",
            MountMode::Rw => "rw",
        })
    }
}

/// A validated host directory that will be bind-mounted 1:1.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Mount {
    /// Absolute path as seen by the user *and* inside the container.
    pub path: PathBuf,
    /// Canonical host path used as the bind source.
    pub source: PathBuf,
    /// Access mode.
    pub mode: MountMode,
}

impl Mount {
    /// The `docker run --mount` value for this mount.
    pub fn docker_mount_arg(&self) -> String {
        let mut s = format!(
            "type=bind,source={},target={}",
            self.source.display(),
            container_path(&self.path)
        );
        if self.mode == MountMode::Ro {
            s.push_str(",readonly");
        }
        s
    }
}

/// `C:\Users\me\p` → `/c/Users/me/p`. Paths without a drive letter only
/// get their separators converted.
pub fn windows_to_container(p: &str) -> String {
    let p = p.strip_prefix(r"\\?\").unwrap_or(p);
    let b = p.as_bytes();
    let (head, rest) = if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        (format!("/{}", (b[0] as char).to_ascii_lowercase()), &p[2..])
    } else {
        (String::new(), p)
    };
    let rest = rest.replace('\\', "/");
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        if head.is_empty() { "/".into() } else { head }
    } else if rest.starts_with('/') {
        format!("{head}{rest}")
    } else {
        format!("{head}/{rest}")
    }
}

/// `/c/Users/me/p` → `C:\Users\me\p`; `None` for paths without a drive.
pub fn container_to_windows(p: &str) -> Option<String> {
    let rest = p.strip_prefix('/')?;
    let mut parts = rest.splitn(2, '/');
    let drive = parts.next()?;
    if drive.len() != 1 || !drive.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    let tail = parts.next().unwrap_or("").replace('/', "\\");
    Some(format!("{}:\\{tail}", drive.to_ascii_uppercase()))
}

/// The path inside the (Linux) container for a host path.
pub fn container_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        windows_to_container(&s)
    } else {
        s.into_owned()
    }
}

/// The host path for a path inside the container.
pub fn host_path(c: &str) -> PathBuf {
    if cfg!(windows) {
        container_to_windows(c)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(c))
    } else {
        PathBuf::from(c)
    }
}

/// `file://` URI for a host path.
pub fn file_uri(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// All problems found while validating mount lists (reported together).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountErrors(pub Vec<String>);

impl fmt::Display for MountErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "invalid mount configuration:")?;
        for e in &self.0 {
            writeln!(f, "  - {e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for MountErrors {}

/// Which host paths may never be mounted (nor any parent of them).
#[derive(Debug, Clone)]
pub struct MountPolicy {
    /// The user's home directory (itself never mountable).
    pub home: PathBuf,
    /// Paths that may not be mounted, nor mounted *through* a parent directory.
    pub protected: Vec<PathBuf>,
    /// Paths that may not be mounted, nor anything beneath them.
    pub system: Vec<PathBuf>,
}

impl MountPolicy {
    /// Default policy: credentials, docker socket and roc's own state are off limits.
    pub fn new(home: &Path, state_dir: &Path, extra_denied: &[String]) -> Self {
        let mut protected: Vec<PathBuf> = [
            ".ssh",
            ".gnupg",
            ".aws",
            ".azure",
            ".kube",
            ".docker",
            ".config/gcloud",
            ".config/gh",
            ".password-store",
            ".local/share/keyrings",
            "Library/Keychains",
        ]
        .iter()
        .map(|p| home.join(p))
        .collect();
        if cfg!(windows) {
            protected.push(home.join("AppData"));
        }
        protected.push(normalize_lexical(state_dir));
        protected.push(PathBuf::from("/var/run/docker.sock"));
        protected.push(PathBuf::from("/run/docker.sock"));
        for e in extra_denied {
            protected.push(normalize_lexical(&expand_tilde(e, home)));
        }
        let unix_system = [
            "/etc",
            "/bin",
            "/sbin",
            "/usr",
            "/lib",
            "/lib64",
            "/boot",
            "/dev",
            "/proc",
            "/sys",
            "/System",
            "/Library",
            "/private/etc",
            "/Applications",
        ];
        let windows_system = [
            r"C:\Windows",
            r"C:\Program Files",
            r"C:\Program Files (x86)",
            r"C:\ProgramData",
        ];
        let system = if cfg!(windows) {
            &windows_system[..]
        } else {
            &unix_system[..]
        }
        .iter()
        .map(PathBuf::from)
        .collect();
        MountPolicy {
            home: home.to_path_buf(),
            protected,
            system,
        }
    }

    /// Returns a reason when `p` must not be mounted.
    pub fn check(&self, p: &Path) -> Option<String> {
        if p.parent().is_none() {
            return Some("the filesystem root cannot be mounted".into());
        }
        if p == self.home {
            return Some("your home directory cannot be mounted as a whole; mount specific project directories".into());
        }
        for s in &self.system {
            if p.starts_with(s) {
                return Some(format!("system directory {} cannot be mounted", s.display()));
            }
        }
        for prot in &self.protected {
            if p.starts_with(prot) {
                return Some(format!("{} is protected", prot.display()));
            }
            if prot.starts_with(p) {
                return Some(format!("it contains the protected path {}", prot.display()));
            }
        }
        None
    }
}

/// Splits a comma separated list, trimming whitespace and dropping empty items.
pub fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Expands a leading `~` or `~/` to `home`. Other paths are returned unchanged.
pub fn expand_tilde(s: &str, home: &Path) -> PathBuf {
    if s == "~" {
        home.to_path_buf()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(s)
    }
}

/// Resolves `.` and `..` lexically (no filesystem access).
pub fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("/");
                }
                if out.as_os_str().is_empty() {
                    out.push("/");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

/// Makes `raw` absolute (expanding `~`, resolving against `cwd`) and normalises it.
pub fn absolutize(raw: &str, cwd: &Path, home: &Path) -> PathBuf {
    let p = expand_tilde(raw, home);
    let p = if p.is_absolute() { p } else { cwd.join(p) };
    normalize_lexical(&p)
}

fn validate_one(raw: &str, mode: MountMode, cwd: &Path, policy: &MountPolicy) -> Result<Mount, String> {
    if raw.chars().any(|c| c.is_control() || c == '"' || c == '=') {
        return Err(format!("{raw:?}: contains a control character, quote or '='"));
    }
    if raw.starts_with('~') && raw != "~" && !raw.starts_with("~/") {
        return Err(format!("{raw:?}: ~user expansion is not supported"));
    }
    let path = absolutize(raw, cwd, &policy.home);
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {}", path.display(), describe_io(&e)))?;
    if !meta.is_dir() {
        return Err(format!("{}: not a directory", path.display()));
    }
    let source = crate::util::canonicalize(&path).map_err(|e| format!("{}: {}", path.display(), describe_io(&e)))?;
    for candidate in [&path, &source] {
        if let Some(reason) = policy.check(candidate) {
            return Err(format!("{}: {reason}", path.display()));
        }
    }
    Ok(Mount { path, source, mode })
}

fn describe_io(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "does not exist".into(),
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        _ => e.to_string(),
    }
}

/// Validates the read/write lists and returns mounts ordered parent-first.
///
/// Every problem is collected and returned at once so the user can fix them in
/// one go. Nothing touches Docker until this succeeds.
pub fn resolve_mounts(
    read: &[String],
    write: &[String],
    cwd: &Path,
    policy: &MountPolicy,
) -> Result<Vec<Mount>, MountErrors> {
    let mut errors = Vec::new();
    let mut mounts: Vec<Mount> = Vec::new();
    let entries = write
        .iter()
        .map(|r| (r, MountMode::Rw))
        .chain(read.iter().map(|r| (r, MountMode::Ro)));
    for (raw, mode) in entries {
        match validate_one(raw, mode, cwd, policy) {
            Ok(m) => {
                if let Some(existing) = mounts.iter().find(|e| e.path == m.path) {
                    if existing.mode != m.mode {
                        errors.push(format!(
                            "{}: listed as both a read dir and a write dir",
                            m.path.display()
                        ));
                    }
                    continue;
                }
                mounts.push(m);
            }
            Err(e) => errors.push(e),
        }
    }
    if !errors.is_empty() {
        return Err(MountErrors(errors));
    }
    mounts.sort_by(|a, b| {
        a.path
            .components()
            .count()
            .cmp(&b.path.components().count())
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(mounts)
}

/// Finds the mount that governs `p` (deepest containing mount).
pub fn governing_mount<'a>(mounts: &'a [Mount], p: &Path) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|m| p.starts_with(&m.path) || p.starts_with(&m.source))
        .max_by_key(|m| m.path.components().count())
}

/// Chooses the container working directory.
///
/// Order: explicit `-workdir` (must be inside a mount) → the current directory
/// if it is inside a mount → the first write mount → the first mount.
pub fn pick_workdir(cwd: &Path, mounts: &[Mount], explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(w) = explicit {
        return if governing_mount(mounts, w).is_some() {
            Ok(w.to_path_buf())
        } else {
            Err(format!("-workdir {} is not inside any mounted directory", w.display()))
        };
    }
    if let Some(m) = governing_mount(mounts, cwd) {
        // Express the cwd in terms of the 1:1 container path.
        let join = |rel: &Path| {
            if rel.as_os_str().is_empty() {
                m.path.clone()
            } else {
                m.path.join(rel)
            }
        };
        if let Ok(rel) = cwd.strip_prefix(&m.path) {
            return Ok(join(rel));
        }
        if let Ok(rel) = cwd.strip_prefix(&m.source) {
            return Ok(join(rel));
        }
    }
    mounts
        .iter()
        .find(|m| m.mode == MountMode::Rw)
        .or_else(|| mounts.first())
        .map(|m| m.path.clone())
        .ok_or_else(|| "no directories to mount".to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn policy(home: &Path) -> MountPolicy {
        MountPolicy::new(home, &home.join(".local/roc"), &[])
    }

    #[test]
    fn csv_split_trims_and_drops_empty() {
        assert_eq!(split_csv(" a , b,,c ,"), vec!["a", "b", "c"]);
        assert!(split_csv("").is_empty());
        assert!(split_csv(" , ").is_empty());
    }

    #[test]
    fn tilde_expansion() {
        let h = Path::new("/Users/andrei");
        assert_eq!(expand_tilde("~", h), PathBuf::from("/Users/andrei"));
        assert_eq!(expand_tilde("~/work", h), PathBuf::from("/Users/andrei/work"));
        assert_eq!(expand_tilde("/abs", h), PathBuf::from("/abs"));
        assert_eq!(expand_tilde("rel/x", h), PathBuf::from("rel/x"));
    }

    #[test]
    fn lexical_normalization() {
        assert_eq!(normalize_lexical(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize_lexical(Path::new("/../..")), PathBuf::from("/"));
        assert_eq!(normalize_lexical(Path::new("/a/b/")), PathBuf::from("/a/b"));
    }

    #[test]
    fn policy_blocks_dangerous_paths() {
        let home = Path::new("/Users/andrei");
        let p = policy(home);
        assert!(p.check(Path::new("/")).is_some());
        assert!(p.check(home).is_some());
        assert!(p.check(Path::new("/Users")).is_some(), "parent of ~/.ssh");
        assert!(p.check(Path::new("/Users/andrei/.ssh")).is_some());
        assert!(p.check(Path::new("/Users/andrei/.ssh/keys")).is_some());
        assert!(
            p.check(Path::new("/Users/andrei/.local")).is_some(),
            "contains roc state"
        );
        assert!(p.check(Path::new("/Users/andrei/.local/roc/sessions")).is_some());
        assert!(p.check(Path::new("/var/run")).is_some(), "contains docker.sock");
        assert!(p.check(Path::new("/etc/nginx")).is_some());
        assert!(p.check(Path::new("/Users/andrei/friends_of/planning")).is_none());
        assert!(p.check(Path::new("/Users/andrei/work")).is_none());
    }

    #[test]
    fn extra_denied_paths_apply() {
        let home = Path::new("/home/u");
        let p = MountPolicy::new(home, &home.join(".local/roc"), &["~/secrets".into()]);
        assert!(p.check(Path::new("/home/u/secrets/x")).is_some());
        assert!(p.check(Path::new("/home/u/code")).is_none());
    }

    #[test]
    fn resolve_valid_mounts_parent_first() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let a = home.join("work");
        let b = home.join("work/project");
        std::fs::create_dir_all(&b).unwrap();
        let pol = policy(&home);
        let mounts = resolve_mounts(
            &["~/work".into()],
            &[b.to_string_lossy().into(), b.to_string_lossy().into()],
            tmp.path(),
            &pol,
        )
        .unwrap();
        assert_eq!(mounts.len(), 2, "duplicates collapse");
        assert_eq!(mounts[0].path, a);
        assert_eq!(mounts[0].mode, MountMode::Ro);
        assert_eq!(mounts[1].path, b);
        assert_eq!(mounts[1].mode, MountMode::Rw);
    }

    #[test]
    fn resolve_reports_all_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let ok = home.join("ok");
        std::fs::create_dir_all(&ok).unwrap();
        std::fs::write(home.join("file.txt"), "x").unwrap();
        let pol = policy(&home);
        let err = resolve_mounts(
            &["~/missing".into(), "~/file.txt".into(), "~bob/x".into()],
            &["~".into(), "/".into(), "~/ok".into()],
            tmp.path(),
            &pol,
        )
        .unwrap_err();
        let joined = err.0.join("\n");
        assert_eq!(err.0.len(), 5, "{joined}");
        assert!(joined.contains("does not exist"));
        assert!(joined.contains("not a directory"));
        assert!(joined.contains("~user"));
        assert!(joined.contains("home directory"));
        assert!(joined.contains("filesystem root"));
    }

    #[test]
    fn read_and_write_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join("p")).unwrap();
        let pol = policy(&home);
        let err = resolve_mounts(&["~/p".into()], &["~/p".into()], tmp.path(), &pol).unwrap_err();
        assert!(err.0[0].contains("both"));
    }

    #[test]
    fn symlink_source_is_canonical_but_target_is_typed_path() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let real = home.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, home.join("link")).unwrap();
        let pol = policy(&home);
        let m = resolve_mounts(&[], &["~/link".into()], tmp.path(), &pol).unwrap();
        assert_eq!(m[0].path, home.join("link"));
        assert_eq!(m[0].source, real.canonicalize().unwrap());
    }

    #[test]
    fn symlink_into_protected_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::os::unix::fs::symlink(home.join(".ssh"), home.join("innocent")).unwrap();
        // canonicalize the home so the canonical source is comparable
        let home_c = home.canonicalize().unwrap();
        let pol = policy(&home_c);
        let err = resolve_mounts(
            &[home_c.join("innocent").to_string_lossy().into()],
            &[],
            tmp.path(),
            &pol,
        )
        .unwrap_err();
        assert!(err.0[0].contains("protected"), "{:?}", err);
    }

    #[test]
    fn mount_arg_format() {
        let m = Mount {
            path: "/Users/a/p".into(),
            source: "/Users/a/p".into(),
            mode: MountMode::Ro,
        };
        assert_eq!(
            m.docker_mount_arg(),
            "type=bind,source=/Users/a/p,target=/Users/a/p,readonly"
        );
    }

    #[test]
    fn workdir_selection() {
        let mounts = vec![
            Mount {
                path: "/r".into(),
                source: "/r".into(),
                mode: MountMode::Ro,
            },
            Mount {
                path: "/w".into(),
                source: "/real/w".into(),
                mode: MountMode::Rw,
            },
        ];
        assert_eq!(
            pick_workdir(Path::new("/w/sub"), &mounts, None).unwrap(),
            PathBuf::from("/w/sub")
        );
        assert_eq!(
            pick_workdir(Path::new("/real/w/sub"), &mounts, None).unwrap(),
            PathBuf::from("/w/sub")
        );
        assert_eq!(
            pick_workdir(Path::new("/elsewhere"), &mounts, None).unwrap(),
            PathBuf::from("/w")
        );
        assert!(pick_workdir(Path::new("/x"), &mounts, Some(Path::new("/nope"))).is_err());
        assert_eq!(
            pick_workdir(Path::new("/x"), &mounts, Some(Path::new("/r/a"))).unwrap(),
            PathBuf::from("/r/a")
        );
    }
}

#[cfg(test)]
mod translation_tests {
    use super::*;

    #[test]
    fn windows_paths_map_to_container_paths() {
        assert_eq!(windows_to_container(r"C:\Users\me\proj"), "/c/Users/me/proj");
        assert_eq!(windows_to_container(r"D:\work\"), "/d/work");
        assert_eq!(windows_to_container(r"\\?\C:\Users\me"), "/c/Users/me");
        assert_eq!(windows_to_container("C:/Users/me"), "/c/Users/me");
        assert_eq!(windows_to_container(r"C:\"), "/c");
        assert_eq!(windows_to_container("/already/posix"), "/already/posix");
    }

    #[test]
    fn container_paths_map_back_to_windows() {
        assert_eq!(
            container_to_windows("/c/Users/me/proj").as_deref(),
            Some(r"C:\Users\me\proj")
        );
        assert_eq!(container_to_windows("/d").as_deref(), Some(r"D:\"));
        assert_eq!(container_to_windows("/usr/lib"), None);
        assert_eq!(container_to_windows("relative/x"), None);
        let back = container_to_windows(&windows_to_container(r"E:\a b\c")).unwrap();
        assert_eq!(back, r"E:\a b\c");
    }

    #[test]
    fn platform_mapping_and_uris() {
        if cfg!(windows) {
            assert_eq!(container_path(Path::new(r"C:\x")), "/c/x");
            assert_eq!(host_path("/c/x"), PathBuf::from(r"C:\x"));
        } else {
            assert_eq!(container_path(Path::new("/Users/a/p")), "/Users/a/p");
            assert_eq!(host_path("/Users/a/p"), PathBuf::from("/Users/a/p"));
        }
        assert_eq!(file_uri(Path::new("/Users/a/p")), "file:///Users/a/p");
        assert_eq!(file_uri(Path::new(r"C:\Users\a")), "file:///C:/Users/a");
    }
}
