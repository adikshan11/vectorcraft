//! The folders automation may read and write (#832).
//!
//! `vectorcraft-cli mcp --automation-read-root <dir> --automation-write-root <dir>`, and
//! `vectorcraft --control <port>` with the same flags, confine the files an agent's commands
//! reach; each flag may be given more than once (#3546). [`AutomationRoots`] holds the folders of
//! each kind. While they are in force, every path a command reads goes through [`check_read`] and
//! every path it writes through [`check_write`]:
//!
//! - The path is made absolute against the working directory, as the file system reads it. Its
//!   links are followed: for a file that doesn't exist yet, the links of its nearest existing
//!   folder, the rest of the path being plain names (`..` below a missing folder is refused, and so
//!   is a link that leads nowhere).
//! - The result must lie inside one of the roots of its kind, compared by whole components
//!   (`/work2` isn't inside `/work`), exactly apart from the case of a Windows drive or share prefix.
//! - On Windows a network path (`\\server\share`, `\\?\UNC\...`, WebDAV's `\\host@SSL\share`), a
//!   device path (`\\.\...`) or a verbatim path that isn't a disk is refused by its name before
//!   the file system is asked about it, unless a root is on that same server and share (or that
//!   same device or prefix). Looking such a path up would contact a host the agent chose. A link
//!   whose text leads to one is refused before it is followed.
//! - Read and write access are separate: a root given alone grants only its own kind of access.
//!
//! Roots are in force on a thread inside [`confine`], and in the whole process after
//! [`confine_process`] (`vectorcraft-cli mcp`, the desktop app with a control port), except inside
//! [`unconfined`]: what the person at the keyboard does, and the app's own folders (preferences,
//! Data Recovery, the library folders, VectorCraft's Fonts folder), which no agent names. Without
//! roots nothing is checked.
//!
//! A path is checked, then opened: a process that can change the folders in between (swapping in
//! a link) can still win that race. The roots keep an agent to the files it was given; they don't
//! guard against other programs on the computer.
//!
//! [`ScriptScope`] holds the folders a running script's own files (its includes and `$.evalFile`)
//! are read from, inside the roots.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Read access or write access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// The folders automation may read and write; a kind with no root grants no access of its kind.
#[derive(Debug)]
pub struct AutomationRoots {
    read: Vec<Root>,
    write: Vec<Root>,
}

#[derive(Debug)]
struct Root {
    /// The folder made absolute, links followed: what paths are compared with.
    resolved: PathBuf,
    /// The same without Windows' `\\?\` prefix, for people.
    shown: PathBuf,
}

impl Root {
    /// The folder at `path`, which must exist.
    fn open(path: &Path, access: Access) -> Result<Self, String> {
        let kind = match access {
            Access::Read => "read",
            Access::Write => "write",
        };
        let resolved = std::fs::canonicalize(path)
            .and_then(|p| if p.is_dir() { Ok(p) } else { Err(std::io::ErrorKind::NotADirectory.into()) })
            .map_err(|e| format!("cannot open the automation {kind} root `{}`: {e}", path.display()))?;
        Ok(Self { shown: crate::cmd::findfiles::plain(resolved.clone()), resolved })
    }
}

impl AutomationRoots {
    /// The roots `read` and `write`, which must be existing folders (one given twice counts once);
    /// `None` when neither kind has any (nothing is confined then).
    pub fn new(read: &[&Path], write: &[&Path]) -> Result<Option<Arc<Self>>, String> {
        if read.is_empty() && write.is_empty() {
            return Ok(None);
        }
        let open = |paths: &[&Path], access: Access| -> Result<Vec<Root>, String> {
            let mut roots: Vec<Root> = Vec::with_capacity(paths.len());
            for path in paths {
                let root = Root::open(path, access)?;
                if !roots.iter().any(|r| r.resolved == root.resolved) {
                    roots.push(root);
                }
            }
            Ok(roots)
        };
        Ok(Some(Arc::new(Self { read: open(read, Access::Read)?, write: open(write, Access::Write)? })))
    }

    /// Roots that grant no access of either kind: inside [`confine`] with them, every path a
    /// command reads or writes is refused.
    pub(crate) fn none() -> Arc<Self> {
        Arc::new(Self { read: Vec::new(), write: Vec::new() })
    }

    /// May the file or folder at `path` be read (or written)? It must lie inside one of the roots
    /// of that kind. `Err` says why not.
    pub fn check(&self, path: &str, access: Access) -> Result<(), String> {
        let roots = match access {
            Access::Read => &self.read,
            Access::Write => &self.write,
        };
        // The messages are spelled out whole, as the translation catalogs match them.
        if roots.is_empty() {
            return match access {
                Access::Read => Err(format!("automation filesystem access is not granted: read authority is absent: {path}")),
                Access::Write => Err(format!("automation filesystem access is not granted: write authority is absent: {path}")),
            };
        }
        let folders: Vec<&Path> = roots.iter().map(|r| r.resolved.as_path()).collect();
        let resolved = resolve(path, &folders)?;
        if roots.iter().any(|r| inside(&resolved, &r.resolved)) {
            return Ok(());
        }
        let shown: Vec<String> = roots.iter().map(|r| r.shown.display().to_string()).collect();
        match (shown.as_slice(), access) {
            ([root], Access::Read) => Err(format!("automation path rejected: outside the read root {root}: {path}")),
            ([root], Access::Write) => Err(format!("automation path rejected: outside the write root {root}: {path}")),
            (_, Access::Read) => Err(format!("automation path rejected: outside the read roots {roots}: {path}", roots = shown.join(", "))),
            (_, Access::Write) => Err(format!("automation path rejected: outside the write roots {roots}: {path}", roots = shown.join(", "))),
        }
    }

    /// The roots for people: `read A, B; write C` (`none` for a kind without any).
    pub fn describe(&self) -> String {
        let list = |roots: &[Root]| {
            if roots.is_empty() { "none".to_string() } else { roots.iter().map(|r| r.shown.display().to_string()).collect::<Vec<_>>().join(", ") }
        };
        format!("read {}; write {}", list(&self.read), list(&self.write))
    }
}

/// Where the roots are in force on a thread ([`confine`], [`unconfined`]).
#[derive(Clone, Debug)]
enum Scope {
    Confined(Arc<AutomationRoots>),
    Unconfined,
}

thread_local! {
    /// This thread's scope; `None`: the process's roots ([`confine_process`]).
    static SCOPE: RefCell<Option<Scope>> = const { RefCell::new(None) };
}

/// The roots in force in every thread outside [`confine`] and [`unconfined`].
static PROCESS: OnceLock<Arc<AutomationRoots>> = OnceLock::new();

/// Put `roots` in force in the whole process, for good. `false` when roots were in force already
/// (they stay as they were).
pub fn confine_process(roots: Arc<AutomationRoots>) -> bool {
    PROCESS.set(roots).is_ok()
}

/// Puts the scope it replaced back when dropped (also when `f` panics).
struct Restore(Option<Scope>);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0.take();
        SCOPE.with(|s| *s.borrow_mut() = previous);
    }
}

fn with_scope<T>(scope: Option<Scope>, f: impl FnOnce() -> T) -> T {
    let _restore = Restore(SCOPE.with(|s| s.replace(scope)));
    f()
}

/// Run `f` with `roots` in force on this thread (`None`: as things stand).
pub fn confine<T>(roots: Option<&Arc<AutomationRoots>>, f: impl FnOnce() -> T) -> T {
    match roots {
        Some(r) => with_scope(Some(Scope::Confined(r.clone())), f),
        None => f(),
    }
}

/// Run `f` with no roots in force on this thread: what the person at the keyboard does, and the
/// app's own folders.
pub fn unconfined<T>(f: impl FnOnce() -> T) -> T {
    with_scope(Some(Scope::Unconfined), f)
}

/// This thread's scope, to carry to work it hands to another thread ([`ScopeHandle::run`]).
#[derive(Clone, Debug)]
pub struct ScopeHandle(Option<Scope>);

impl ScopeHandle {
    /// The scope of the calling thread.
    pub fn capture() -> Self {
        Self(SCOPE.with(|s| s.borrow().clone()))
    }

    /// Run `f` in the captured scope.
    pub fn run<T>(self, f: impl FnOnce() -> T) -> T {
        with_scope(self.0, f)
    }
}

/// The roots in force on this thread, if any.
pub fn current() -> Option<Arc<AutomationRoots>> {
    match SCOPE.with(|s| s.borrow().clone()) {
        Some(Scope::Confined(r)) => Some(r),
        Some(Scope::Unconfined) => None,
        None => PROCESS.get().cloned(),
    }
}

/// May `path` be read here? Always, without roots in force.
pub fn check_read(path: &str) -> Result<(), String> {
    current().map_or(Ok(()), |r| r.check(path, Access::Read))
}

/// Is `path`, under a read root, written as an absolute path of another platform: outside Windows, one
/// that starts with a drive (`C:`) or a `\`? [`check_read`] reads it as a name in the working folder.
pub fn written_elsewhere(path: &str) -> bool {
    let foreign = path.starts_with('\\') || matches!(path.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic());
    !cfg!(windows) && foreign && current().is_some()
}

/// Does a command param named `key` hold a file or folder path: `path`, `paths`, `file`, `files`,
/// `folder`, or a name ending in `Path`, `Paths`, `File`, `Files` or `Folder` (`diffPath`,
/// `destinationFolder`)?
pub fn is_path_key(key: &str) -> bool {
    matches!(key, "path" | "paths" | "file" | "files" | "folder") || ["Path", "Paths", "File", "Files", "Folder"].iter().any(|end| key.ends_with(end))
}

/// May `path` be written here? Always, without roots in force.
pub fn check_write(path: &str) -> Result<(), String> {
    current().map_or(Ok(()), |r| r.check(path, Access::Write))?;
    WRITES.with(|w| {
        if let Some(paths) = w.borrow_mut().as_mut()
            && !paths.iter().any(|(p, _)| p == path)
        {
            paths.push((path.to_string(), stamp(path)));
        }
    });
    Ok(())
}

/// A file's modification time and length; `None` when there is no file at `path`.
type Stamp = Option<(Option<std::time::SystemTime>, u64)>;

fn stamp(path: &str) -> Stamp {
    std::fs::metadata(path).ok().filter(std::fs::Metadata::is_file).map(|m| (m.modified().ok(), m.len()))
}

thread_local! {
    /// The paths [`check_write`] allowed on this thread while they are being recorded, each with
    /// its file's stamp at that moment.
    static WRITES: std::cell::RefCell<Option<Vec<(String, Stamp)>>> = const { std::cell::RefCell::new(None) };
}

/// Starts recording the paths [`check_write`] allows on this thread.
pub(crate) fn start_recording_writes() {
    WRITES.with(|w| *w.borrow_mut() = Some(Vec::new()));
}

/// Ends recording → the recorded paths whose file exists now: after a command that `succeeded`,
/// all of them; after one that failed, those made or changed since they were allowed.
pub(crate) fn finish_recording_writes(succeeded: bool) -> Vec<String> {
    let recorded = WRITES.with(|w| w.borrow_mut().take()).unwrap_or_default();
    recorded.into_iter().filter(|(path, before)| stamp(path).is_some_and(|now| succeeded || Some(now) != *before)).map(|(path, _)| path).collect()
}

/// `path` made absolute with its links followed; for a path that doesn't exist (yet), its nearest
/// existing folder's links followed and the rest appended.
///
/// A Windows network, device or non-disk verbatim prefix is refused before any of that, unless one
/// of `roots` (resolved folders) already stands on the same server and share, device or prefix.
fn resolve(path: &str, roots: &[&Path]) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("automation path rejected: path is empty".into());
    }
    // Checked as given: making `…\NUL` absolute gives the device path `\\.\NUL` on Windows.
    if cfg!(windows)
        && let Some(why) = windows_name_refusal(path)
    {
        return Err(format!("automation path rejected: {why}: {path}"));
    }
    // Before `absolute` too: on Windows that call asks the system about the path.
    if let Some(why) = refused_prefix(Path::new(path), roots) {
        return Err(format!("automation path rejected: {why}: {path}"));
    }
    let absolute = std::path::absolute(path).map_err(|e| format!("automation path rejected: cannot resolve {path}: {e}"))?;
    if let Some(why) = refused_prefix(&absolute, roots) {
        return Err(format!("automation path rejected: {why}: {path}"));
    }
    // A link is judged by its own text: canonicalizing one that leads to a network path would
    // contact that host. Only Windows has such prefixes. One walk of the whole path covers every
    // shorter one the loop below tries.
    if cfg!(windows)
        && let Some(why) = link_leads_to_refused_prefix(&absolute, roots)
    {
        return Err(format!("automation path rejected: {why}: {path}"));
    }
    // The names below the nearest existing folder, last first.
    let mut missing: Vec<OsString> = vec![];
    let mut at = absolute.as_path();
    loop {
        match std::fs::canonicalize(at) {
            Ok(mut found) => {
                found.extend(missing.iter().rev());
                return Ok(found);
            }
            Err(e) => match std::fs::symlink_metadata(at) {
                Ok(m) if m.file_type().is_symlink() => return Err(format!("automation path rejected: a link that leads nowhere: {path}")),
                // There, yet its real path can't be had (no permission): nothing to compare.
                Ok(_) => return Err(format!("automation path rejected: cannot resolve {path}: {e}")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("automation path rejected: cannot resolve {path}: {e}")),
            },
        }
        let parent = at.parent();
        match (at.components().next_back(), parent) {
            (Some(Component::Normal(name)), Some(parent)) => {
                missing.push(name.to_os_string());
                at = parent;
            }
            (Some(Component::ParentDir), _) => {
                return Err(format!("automation path rejected: parent traversal below a folder that doesn't exist: {path}"));
            }
            // Down to the root (or drive) and it doesn't exist either.
            _ => {
                let e = std::io::Error::from(std::io::ErrorKind::NotFound);
                return Err(format!("automation path rejected: cannot resolve {path}: {e}"));
            }
        }
    }
}

/// Is `path` (resolved) `root` or inside it, component by component (without regard to case in a
/// Windows drive or share prefix only: resolved folder names have their real case, and a folder that differs
/// in case alone may be another, on a case-sensitive Windows folder)?
fn inside(path: &Path, root: &Path) -> bool {
    let mut path = path.components();
    root.components().all(|r| path.next().is_some_and(|p| same_component(p, r)))
}

fn same_component(a: Component, b: Component) -> bool {
    if a == b {
        return true;
    }
    if !cfg!(windows) || !matches!((a, b), (Component::Prefix(_), Component::Prefix(_))) {
        return false;
    }
    match (a.as_os_str().to_str(), b.as_os_str().to_str()) {
        (Some(a), Some(b)) => a.to_lowercase() == b.to_lowercase(),
        _ => false,
    }
}

/// A Windows device name (`CON`, `NUL`, `COM1`, `COM¹`… also with an extension), which names no file.
pub(crate) fn is_device_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component).trim_end_matches(' ').to_ascii_uppercase();
    let numbered =
        |prefix: &str| stem.strip_prefix(prefix).is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"));
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT")
}

/// A Windows path prefix that isn't a local disk: opening or even resolving a path that starts
/// with one can reach another computer.
#[derive(Debug)]
enum OffDisk {
    /// `\\server\share` or `\\?\UNC\server\share`; WebDAV's `\\host@SSL\share` is a server name too.
    Unc(OsString, OsString),
    /// `\\.\device`.
    Device(OsString),
    /// `\\?\` followed by anything but a drive or `UNC`.
    Verbatim(OsString),
}

impl OffDisk {
    /// The prefix of `path` when it isn't a local disk. A drive letter, a path without a prefix
    /// and every path off Windows give `None`.
    fn of(path: &Path) -> Option<Self> {
        use std::path::Prefix;
        let Some(Component::Prefix(prefix)) = path.components().next() else { return Self::unparsed_unc(path) };
        match prefix.kind() {
            Prefix::Disk(_) | Prefix::VerbatimDisk(_) => None,
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => Some(Self::Unc(server.to_os_string(), share.to_os_string())),
            Prefix::DeviceNS(device) => Some(Self::Device(device.to_os_string())),
            Prefix::Verbatim(rest) => Some(Self::Verbatim(rest.to_os_string())),
        }
    }

    /// A path that starts with two separators on Windows but isn't a whole `\\server\share` (`\\server`,
    /// `\\\x`): Windows still reads it as the name of a computer, so it is a UNC path with no share.
    fn unparsed_unc(path: &Path) -> Option<Self> {
        let bytes = path.as_os_str().as_encoded_bytes();
        let separator = |b: Option<&u8>| matches!(b, Some(b'\\' | b'/'));
        if cfg!(windows) && separator(bytes.first()) && separator(bytes.get(1)) { Some(Self::Unc(OsString::new(), OsString::new())) } else { None }
    }

    /// The refusal for a path with this prefix, which names the prefix.
    fn refusal(&self) -> &'static str {
        match self {
            Self::Unc(..) => "UNC paths are refused unless a root is on the same server and share",
            Self::Device(_) => "device paths are refused unless a root uses the same device",
            Self::Verbatim(_) => "verbatim paths are refused unless a root uses the same prefix",
        }
    }

    /// Whether `other` is the same server and share, device or prefix, the ASCII case aside. This
    /// decides only whether that host may be contacted; [`inside`] still decides the folder.
    fn same_as(&self, other: &Self) -> bool {
        let same = |a: &OsString, b: &OsString| a.eq_ignore_ascii_case(b);
        match (self, other) {
            (Self::Unc(server, share), Self::Unc(other_server, other_share)) => same(server, other_server) && same(share, other_share),
            (Self::Device(device), Self::Device(other)) => same(device, other),
            (Self::Verbatim(rest), Self::Verbatim(other)) => same(rest, other),
            _ => false,
        }
    }
}

/// Why `path` must not be looked up: it starts with a Windows network, device or non-disk
/// verbatim prefix and none of `roots` stands on that same prefix. A local path gives `None`.
fn refused_prefix(path: &Path, roots: &[&Path]) -> Option<&'static str> {
    let prefix = OffDisk::of(path)?;
    if roots.iter().filter_map(|root| OffDisk::of(root)).any(|root| root.same_as(&prefix)) { None } else { Some(prefix.refusal()) }
}

/// How many links in a row [`link_leads_to_refused_prefix`] follows before it gives up and refuses.
const MAX_LINK_HOPS: usize = 40;

/// How many spellings of a path one walk scans in all, so a hostile set of links can't make it
/// scan without end.
const MAX_SPELLINGS: usize = 1024;

/// The refusal for a link in `path`, taken from the link's own text.
///
/// Folders are inspected from the top down with [`std::fs::symlink_metadata`], which doesn't follow
/// a link, and a link's target is read with [`std::fs::read_link`]. A link to a network path is
/// therefore refused before [`std::fs::canonicalize`] could open that host, also when the link is
/// only an ancestor of the name. A link whose target can't be read, the one after
/// [`MAX_LINK_HOPS`] in a row, or a walk past [`MAX_SPELLINGS`] spellings, is refused rather than
/// opened.
fn link_leads_to_refused_prefix(path: &Path, roots: &[&Path]) -> Option<&'static str> {
    follow_links(path, roots, &mut std::collections::HashSet::new(), MAX_LINK_HOPS)
}

/// [`link_leads_to_refused_prefix`] for one spelling of the path. `judged` holds every spelling the
/// walk has scanned (so none is scanned twice, nor in a cycle forever) and `hops_left` the links
/// still allowed in this chain, which the other spelling of a target does not use up.
fn follow_links(path: &Path, roots: &[&Path], judged: &mut std::collections::HashSet<PathBuf>, hops_left: usize) -> Option<&'static str> {
    if !judged.insert(path.to_path_buf()) {
        return None;
    }
    if judged.len() > MAX_SPELLINGS {
        return Some("too many links");
    }
    let mut ancestors: Vec<&Path> = path.ancestors().collect();
    ancestors.reverse();
    for prefix in ancestors {
        if prefix.as_os_str().is_empty() {
            continue;
        }
        if let Some(why) = refused_prefix(prefix, roots) {
            return Some(why);
        }
        let meta = match std::fs::symlink_metadata(prefix) {
            Ok(meta) => meta,
            // Nothing is there, so no link is below it.
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => return None,
            // Anything else (no permission to look): a link below it can't be seen, so don't go on.
            Err(_) => return Some("a folder that can't be looked at"),
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        if hops_left == 0 {
            return Some("too many links");
        }
        let Ok(target) = std::fs::read_link(prefix) else { return Some("a link that leads nowhere") };
        // Judged as written: joined onto the link's folder, a bare `\\server` would keep that folder's drive.
        if let Some(why) = refused_prefix(&target, roots) {
            return Some(why);
        }
        let joined = match prefix.parent() {
            Some(parent) if !target.is_absolute() && !parent.as_os_str().is_empty() => parent.join(target),
            _ => target,
        };
        let next = match path.strip_prefix(prefix) {
            Ok(rest) if !rest.as_os_str().is_empty() => joined.join(rest),
            _ => joined,
        };
        if let Some(why) = refused_prefix(&next, roots) {
            return Some(why);
        }
        // Windows takes the `..` of a relative target by name, so `missing\..\remote` is `remote`
        // though `missing` isn't there: judge that spelling as well as the one written.
        let by_name = lexical(&next);
        if by_name != next
            && let Some(why) = follow_links(&by_name, roots, judged, hops_left - 1)
        {
            return Some(why);
        }
        return follow_links(&next, roots, judged, hops_left - 1);
    }
    None
}

/// `path` with its `.` and `..` taken by name, never above its root.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Why Windows would not take `path` as an ordinary file name, for the first of its components
/// that is a device name, or holds a `:` (a drive letter's own colon belongs to the prefix, not to
/// a name, so only a stream such as `x.svg:evil` has one). (Judged by name alone, so it can be
/// tested on any platform; `resolve` applies it on Windows only.)
fn windows_name_refusal(path: &str) -> Option<&'static str> {
    Path::new(path).components().find_map(|c| match c {
        Component::Normal(n) if is_device_name(&n.to_string_lossy()) => Some("reserved device names are not allowed"),
        Component::Normal(n) if n.to_string_lossy().contains(':') => Some("alternate data streams are not allowed"),
        _ => None,
    })
}

/// The folders a running script's own files are read from: the files its `#include` lines name and
/// the files it passes to `$.evalFile` (`Scope::Script`'s reads). VectorCraft's own rules: a script
/// run from a file may read inside that file's folder and the folders below it, and a script given as
/// text may read none. A path is refused when it has a `..` component, a Windows device name or
/// stream, or names a computer on the network, or when it leads (through its links, junctions
/// included) out of every granted folder. The automation roots ([`check_read`]) apply as well.
///
/// The file opened is the path with its links followed, and after reading, the path is followed
/// again and must still lead to the same place: a process that changes the folders twice in between
/// can still win that race.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScriptScope {
    /// The granted folders, made absolute with their links followed.
    roots: Vec<PathBuf>,
}

/// Why a script's file was not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScriptRead {
    /// There is no such file.
    Missing,
    /// The scope does not let the script read it.
    Refused,
    /// The file has more than the bytes asked for.
    TooLarge,
    /// The file system gave this error.
    Failed(String),
}

impl ScriptScope {
    /// The scope of a script run from a file in folder `dir`: that folder.
    pub(crate) fn of_folder(dir: &Path) -> Self {
        Self { roots: std::fs::canonicalize(dir).ok().into_iter().collect() }
    }

    /// Is `path` refused by its name alone: a `..` component, a NUL, a Windows device name or
    /// stream, a network, device or non-disk verbatim prefix, or a link whose text leads to one?
    /// A script's own folder grants no network share, so none is let through.
    fn refused_by_name(path: &Path) -> bool {
        let text = path.to_string_lossy();
        text.contains('\0')
            || path.components().any(|c| c == Component::ParentDir)
            || (cfg!(windows)
                && (windows_name_refusal(&text).is_some() || OffDisk::of(path).is_some() || link_leads_to_refused_prefix(path, &[]).is_some()))
    }

    /// The real path of the file at `path` and its first `max` bytes, at most.
    pub(crate) fn read(&self, path: &Path, max: usize) -> Result<(PathBuf, Vec<u8>), ScriptRead> {
        use std::io::Read;
        if self.roots.is_empty() || Self::refused_by_name(path) || check_read(&path.to_string_lossy()).is_err() {
            return Err(ScriptRead::Refused);
        }
        let real = match std::fs::canonicalize(path) {
            Ok(real) => real,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A link whose target is missing is there all the same: it is not followed.
                return Err(if std::fs::symlink_metadata(path).is_ok() { ScriptRead::Refused } else { ScriptRead::Missing });
            }
            Err(e) => return Err(ScriptRead::Failed(e.to_string())),
        };
        if !self.roots.iter().any(|r| inside(&real, r)) {
            return Err(ScriptRead::Refused);
        }
        let file = std::fs::File::open(&real).map_err(|e| ScriptRead::Failed(e.to_string()))?;
        let meta = file.metadata().map_err(|e| ScriptRead::Failed(e.to_string()))?;
        if !meta.is_file() {
            return Err(ScriptRead::Missing);
        }
        if meta.len() > max as u64 {
            return Err(ScriptRead::TooLarge);
        }
        let mut bytes = Vec::new();
        // One byte more than asked for shows a file that grew while it was read.
        file.take(max as u64 + 1).read_to_end(&mut bytes).map_err(|e| ScriptRead::Failed(e.to_string()))?;
        if bytes.len() > max {
            return Err(ScriptRead::TooLarge);
        }
        if std::fs::canonicalize(path).ok().as_ref() != Some(&real) {
            return Err(ScriptRead::Refused);
        }
        Ok((real, bytes))
    }

    /// The first granted folder, as people read it (`None` for a script given as text).
    pub(crate) fn folder(&self) -> Option<PathBuf> {
        self.roots.first().map(|r| crate::cmd::findfiles::plain(r.clone()))
    }

    /// Does the absolute path `path` name a granted folder or a place inside one, by its names
    /// alone, without regard to case? A path that does not is refused before the file system is
    /// asked anything about it.
    fn names_inside(&self, path: &Path) -> bool {
        let lower = |c: Component| c.as_os_str().to_string_lossy().to_lowercase();
        path.is_absolute()
            && self.roots.iter().any(|r| {
                let root = crate::cmd::findfiles::plain(r.clone());
                let mut names = path.components();
                root.components().all(|c| names.next().is_some_and(|n| lower(n) == lower(c)))
            })
    }

    /// The real path of the file or folder at `path`, which must be absolute: refused as
    /// [`ScriptScope::read`] refuses a path, and also when its names alone place it outside every
    /// granted folder, in which case the file system is not asked about it.
    pub(crate) fn locate(&self, path: &Path) -> Result<PathBuf, ScriptRead> {
        if self.roots.is_empty() || Self::refused_by_name(path) || !self.names_inside(path) {
            return Err(ScriptRead::Refused);
        }
        probed();
        if check_read(&path.to_string_lossy()).is_err() {
            return Err(ScriptRead::Refused);
        }
        let real = match std::fs::canonicalize(path) {
            Ok(real) => real,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(path).is_ok() || !self.missing_inside(path) {
                    return Err(ScriptRead::Refused);
                }
                return Err(ScriptRead::Missing);
            }
            Err(e) => return Err(ScriptRead::Failed(e.to_string())),
        };
        if !self.roots.iter().any(|r| inside(&real, r)) {
            return Err(ScriptRead::Refused);
        }
        Ok(real)
    }

    /// Is the missing `path` inside a granted folder: does the nearest folder above it that exists
    /// lie inside one? On a file system where case matters, `MAIN/x` beside the granted `main` is
    /// outside, whether `MAIN` exists or not. A link above it that leads to nothing is outside too,
    /// so a script cannot tell whether the folder a link leads to exists.
    fn missing_inside(&self, path: &Path) -> bool {
        path.ancestors()
            .skip(1)
            .find(|a| std::fs::symlink_metadata(a).is_ok())
            .and_then(|a| std::fs::canonicalize(a).ok())
            .is_some_and(|real| self.roots.iter().any(|r| inside(&real, r)))
    }

    /// What the file or folder at `path` is, located as [`ScriptScope::locate`] locates it.
    pub(crate) fn stat(&self, path: &Path) -> Result<ScriptEntry, ScriptRead> {
        let real = self.locate(path)?;
        let meta = std::fs::metadata(&real).map_err(|e| ScriptRead::Failed(e.to_string()))?;
        let link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
        #[cfg(windows)]
        let hidden = std::os::windows::fs::MetadataExt::file_attributes(&meta) & 2 != 0;
        #[cfg(not(windows))]
        let hidden = real.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'));
        Ok(ScriptEntry {
            dir: meta.is_dir(),
            file: meta.is_file(),
            len: meta.len(),
            created: meta.created().ok(),
            modified: meta.modified().ok(),
            hidden,
            readonly: meta.permissions().readonly(),
            link,
        })
    }

    /// The names in the folder at `path` that `keep` keeps, with whether each is a folder, in the
    /// order of their names. A name whose links lead out of every granted folder, or out of the
    /// automation read roots in force, is left out. More
    /// than `max` names kept, or more than [`MAX_LISTED`] names looked at, is
    /// [`ScriptRead::TooLarge`].
    pub(crate) fn list(&self, path: &Path, max: usize, keep: impl Fn(&str) -> bool) -> Result<Vec<(String, bool)>, ScriptRead> {
        let real = self.locate(path)?;
        let entries = std::fs::read_dir(&real).map_err(|e| ScriptRead::Failed(e.to_string()))?;
        let mut out = Vec::new();
        for (n, entry) in entries.enumerate() {
            if n >= MAX_LISTED {
                return Err(ScriptRead::TooLarge);
            }
            let Ok(entry) = entry else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if !keep(&name) {
                continue;
            }
            if Self::refused_by_name(&entry.path()) {
                continue;
            }
            let Ok(target) = std::fs::canonicalize(entry.path()) else { continue };
            if !self.roots.iter().any(|r| inside(&target, r)) || check_read(&entry.path().to_string_lossy()).is_err() {
                continue;
            }
            if out.len() >= max {
                return Err(ScriptRead::TooLarge);
            }
            out.push((name, target.is_dir()));
        }
        out.sort();
        Ok(out)
    }
}

/// The most names [`ScriptScope::list`] looks at in one folder.
pub(crate) const MAX_LISTED: usize = 65_536;

/// A file or folder as [`ScriptScope::stat`] gives it.
#[derive(Clone, Debug)]
pub(crate) struct ScriptEntry {
    pub dir: bool,
    /// A regular file.
    pub file: bool,
    pub len: u64,
    pub created: Option<std::time::SystemTime>,
    pub modified: Option<std::time::SystemTime>,
    pub hidden: bool,
    pub readonly: bool,
    /// The path itself is a symbolic link.
    pub link: bool,
}

thread_local! {
    /// How many paths [`ScriptScope::locate`] has asked the file system about on this thread.
    static PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn probed() {
    PROBES.with(|p| p.set(p.get().saturating_add(1)));
}

/// How many paths [`ScriptScope::locate`] has asked the file system about on this thread.
#[cfg(test)]
pub(crate) fn script_probes() -> usize {
    PROBES.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder holding `in` (read root), `out` (write root) and `outside`.
    fn folders(name: &str) -> (PathBuf, Arc<AutomationRoots>) {
        let base = std::env::temp_dir().join(format!("vc-file-access-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in ["in/sub", "out", "outside", "in2"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::write(base.join("in/a.svg"), "<svg/>").unwrap();
        std::fs::write(base.join("outside/secret.svg"), "<svg/>").unwrap();
        let roots = AutomationRoots::new(&[&base.join("in")], &[&base.join("out")]).unwrap().unwrap();
        (base, roots)
    }

    /// `p` as text.
    fn s(p: PathBuf) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn paths_inside_the_roots_pass_and_others_dont() {
        let (base, roots) = folders("inside");
        assert_eq!(roots.check(&s(base.join("in/a.svg")), Access::Read), Ok(()));
        assert_eq!(roots.check(&s(base.join("in")), Access::Read), Ok(()), "the root itself");
        assert_eq!(roots.check(&s(base.join("in/sub/new.svg")), Access::Read), Ok(()), "missing files are checked by their folder");
        assert_eq!(roots.check(&s(base.join("out/new/deeper/x.png")), Access::Write), Ok(()), "and by their nearest folder");
        let e = roots.check(&s(base.join("outside/secret.svg")), Access::Read).unwrap_err();
        assert!(e.starts_with("automation path rejected: outside the read root"), "{e}");
        // Read and write access are separate.
        let e = roots.check(&s(base.join("in/a.svg")), Access::Write).unwrap_err();
        assert!(e.contains("outside the write root"), "{e}");
        assert!(roots.check(&s(base.join("out/x.png")), Access::Read).unwrap_err().contains("outside the read root"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_sibling_with_the_same_prefix_is_outside() {
        let (base, roots) = folders("prefix");
        std::fs::write(base.join("in2/b.svg"), "<svg/>").unwrap();
        assert!(roots.check(&s(base.join("in2/b.svg")), Access::Read).is_err(), "in2 is not inside in");
        assert!(roots.check(&format!("{}2/c.svg", s(base.join("in"))), Access::Read).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn parent_traversal_is_resolved_or_refused() {
        let (base, roots) = folders("dotdot");
        let sep = std::path::MAIN_SEPARATOR;
        let up = format!("{}{sep}..{sep}outside{sep}secret.svg", s(base.join("in")));
        assert!(roots.check(&up, Access::Read).unwrap_err().contains("outside the read root"), "`..` out of the root");
        let back = format!("{}{sep}..{sep}a.svg", s(base.join("in/sub")));
        assert_eq!(roots.check(&back, Access::Read), Ok(()), "`..` that stays inside");
        let missing = format!("{}{sep}nope{sep}..{sep}..{sep}outside{sep}x.svg", s(base.join("out")));
        let e = roots.check(&missing, Access::Write).unwrap_err();
        // Windows resolves `..` by name first (so does opening the file there): then it's outside.
        assert!(e.contains("parent traversal below a folder that doesn't exist") || e.contains("outside the write root"), "{e}");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn relative_paths_resolve_against_the_working_directory() {
        let (base, roots) = folders("relative");
        // The tests' working directory is the crate's folder: not inside the roots.
        assert!(roots.check("a.svg", Access::Read).unwrap_err().contains("outside the read root"));
        assert!(roots.check("", Access::Read).unwrap_err().contains("path is empty"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_missing_root_is_an_error_and_none_is_no_confinement() {
        let (base, _) = folders("missing");
        let e = AutomationRoots::new(&[&base.join("nope")], &[]).unwrap_err();
        assert!(e.starts_with("cannot open the automation read root"), "{e}");
        let e = AutomationRoots::new(&[], &[&base.join("in/a.svg")]).unwrap_err();
        assert!(
            e.starts_with("cannot open the automation write root")
                && e.ends_with(&std::io::Error::from(std::io::ErrorKind::NotADirectory).to_string()),
            "{e}"
        );
        assert!(AutomationRoots::new(&[Path::new("")], &[]).unwrap_err().starts_with("cannot open the automation read root ``"));
        assert!(AutomationRoots::new(&[], &[]).unwrap().is_none());
        // A missing folder among several is an error too.
        assert!(AutomationRoots::new(&[&base.join("in"), &base.join("nope")], &[]).unwrap_err().starts_with("cannot open the automation read root"));
        // One root alone grants only its own access.
        let read_only = AutomationRoots::new(&[&base.join("in")], &[]).unwrap().unwrap();
        let e = read_only.check(&s(base.join("in/x.svg")), Access::Write).unwrap_err();
        assert!(e.contains("write authority is absent"), "{e}");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn links_are_followed_before_comparing() {
        let (base, roots) = folders("links");
        #[cfg(unix)]
        let made = |target: &Path, link: &Path| std::os::unix::fs::symlink(target, link).is_ok();
        #[cfg(windows)]
        let made = |target: &Path, link: &Path| {
            let made =
                if target.is_dir() { std::os::windows::fs::symlink_dir(target, link) } else { std::os::windows::fs::symlink_file(target, link) };
            made.is_ok()
        };
        #[cfg(not(any(unix, windows)))]
        let made = |_: &Path, _: &Path| false;
        // Windows needs Developer Mode or elevation to make links: nothing to test without them.
        if !made(&base.join("outside/secret.svg"), &base.join("in/leak.svg")) {
            eprintln!("skipped: symbolic links can't be made here");
            let _ = std::fs::remove_dir_all(base);
            return;
        }
        assert!(roots.check(&s(base.join("in/leak.svg")), Access::Read).unwrap_err().contains("outside the read root"), "a file link out");
        assert!(made(&base.join("outside"), &base.join("out/away")));
        let e = roots.check(&s(base.join("out/away/new.png")), Access::Write).unwrap_err();
        assert!(e.contains("outside the write root"), "a folder link out: {e}");
        assert!(made(&base.join("outside/gone.svg"), &base.join("out/dangling.png")));
        let e = roots.check(&s(base.join("out/dangling.png")), Access::Write).unwrap_err();
        assert!(e.contains("a link that leads nowhere"), "writing would create the file outside: {e}");
        // A link to a folder inside the root stays inside.
        assert!(made(&base.join("in/sub"), &base.join("in/alias")));
        assert_eq!(roots.check(&s(base.join("in/alias/x.svg")), Access::Read), Ok(()));
        // A root given through a link is the folder it leads to.
        assert!(made(&base.join("in"), &base.join("in-link")));
        let linked = AutomationRoots::new(&[&base.join("in-link")], &[]).unwrap().unwrap();
        assert_eq!(linked.check(&s(base.join("in/a.svg")), Access::Read), Ok(()));
        let _ = std::fs::remove_dir_all(base);
    }

    /// #3546: several roots of a kind: a path inside any of them passes, one inside none is
    /// refused naming them all, and read and write stay separate.
    #[test]
    fn several_roots_of_a_kind_each_grant_their_access() {
        let (base, _) = folders("several");
        std::fs::create_dir_all(base.join("out2")).unwrap();
        std::fs::write(base.join("in2/b.svg"), "<svg/>").unwrap();
        let roots = AutomationRoots::new(&[&base.join("in"), &base.join("in2")], &[&base.join("out"), &base.join("out2")]).unwrap().unwrap();
        assert_eq!(roots.check(&s(base.join("in/a.svg")), Access::Read), Ok(()));
        assert_eq!(roots.check(&s(base.join("in2/b.svg")), Access::Read), Ok(()));
        let e = roots.check(&s(base.join("outside/secret.svg")), Access::Read).unwrap_err();
        assert!(e.starts_with("automation path rejected: outside the read roots ") && e.contains(", "), "{e}");
        assert_eq!(roots.check(&s(base.join("out/x.png")), Access::Write), Ok(()));
        assert_eq!(roots.check(&s(base.join("out2/new/x.png")), Access::Write), Ok(()));
        assert!(roots.check(&s(base.join("outside/x.png")), Access::Write).unwrap_err().contains("outside the write roots"));
        // Read and write access stay separate with several roots.
        assert!(roots.check(&s(base.join("in2/b.svg")), Access::Write).is_err());
        assert!(roots.check(&s(base.join("out2/x.png")), Access::Read).is_err());
        // `..` out of one root into another root's sibling is still outside.
        let sep = std::path::MAIN_SEPARATOR;
        let up = format!("{}{sep}..{sep}outside{sep}secret.svg", s(base.join("in2")));
        assert!(roots.check(&up, Access::Read).is_err());
        // A folder given twice (here once through `..`) counts once.
        let twice = AutomationRoots::new(&[&base.join("in"), &base.join("in/sub/..")], &[]).unwrap().unwrap();
        assert!(!twice.describe().contains(", "), "{}", twice.describe());
        assert!(twice.describe().ends_with("; write none"), "{}", twice.describe());
        let _ = std::fs::remove_dir_all(base);
    }

    /// A link inside one root that leads into another root of the same kind stays inside; one that
    /// leads out of all of them is outside.
    #[cfg(unix)]
    #[test]
    fn links_between_several_roots() {
        let (base, _) = folders("several-links");
        std::fs::write(base.join("in2/b.svg"), "<svg/>").unwrap();
        std::os::unix::fs::symlink(base.join("in2/b.svg"), base.join("in/to-in2.svg")).unwrap();
        std::os::unix::fs::symlink(base.join("outside/secret.svg"), base.join("in2/leak.svg")).unwrap();
        let roots = AutomationRoots::new(&[&base.join("in"), &base.join("in2")], &[]).unwrap().unwrap();
        assert_eq!(roots.check(&s(base.join("in/to-in2.svg")), Access::Read), Ok(()));
        assert!(roots.check(&s(base.join("in2/leak.svg")), Access::Read).unwrap_err().contains("outside the read roots"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(windows)]
    #[test]
    fn windows_ignores_case_and_refuses_devices() {
        let (base, roots) = folders("case");
        let upper = s(base.join("IN/A.SVG"));
        assert_eq!(roots.check(&upper, Access::Read), Ok(()), "{upper}");
        assert_eq!(roots.check(&s(base.join("In/Sub/New.svg")), Access::Read), Ok(()));
        let e = roots.check(&s(base.join("out/NUL")), Access::Write).unwrap_err();
        assert!(e.contains("reserved device names"), "{e}");
        assert!(roots.check(&s(base.join("out/com1.txt")), Access::Write).is_err());
        let e = roots.check(&s(base.join("out/COM¹.txt")), Access::Write).unwrap_err();
        assert!(e.contains("reserved device names"), "{e}");
        assert_eq!(roots.check(&s(base.join("out/console.txt")), Access::Write), Ok(()));
        let _ = std::fs::remove_dir_all(base);
    }

    /// A path with no Windows prefix, a drive letter, and every path off Windows have no off-disk prefix to refuse.
    #[test]
    fn a_local_path_has_no_off_disk_prefix() {
        for p in ["/home/me/a.svg", "a.svg", r"C:\Projects\a.svg", r"\\?\C:\Projects\a.svg"] {
            assert!(OffDisk::of(Path::new(p)).is_none(), "{p}");
            assert_eq!(refused_prefix(Path::new(p), &[]), None, "{p}");
        }
    }

    /// A network, device or non-disk verbatim path is refused by its name before the file system is
    /// asked, so no host the agent names is contacted. The error names the prefix; it is not the
    /// error that opening the path would give. Nothing here touches the network.
    #[cfg(windows)]
    #[test]
    fn a_network_path_is_refused_before_it_is_opened() {
        let (base, roots) = folders("unc");
        let paths = [
            (r"\\203.0.113.5\s\x.svg", "UNC"),
            (r"\\?\UNC\203.0.113.5\s\x.svg", "UNC"),
            ("//203.0.113.5/s/x.svg", "UNC"),
            (r"\\host@SSL\share\x.svg", "UNC"),
            (r"\\host@SSL@443\share\x.svg", "UNC"),
            (r"\\203.0.113.5", "UNC"),
            (r"\\\203.0.113.5\s\x.svg", "UNC"),
            ("//203.0.113.5", "UNC"),
            (r"\\.\PhysicalDrive0", "device path"),
            (r"\\?\Volume{00000000-0000-0000-0000-000000000000}\x.svg", "verbatim path"),
        ];
        for (p, kind) in paths {
            for access in [Access::Read, Access::Write] {
                let e = roots.check(p, access).unwrap_err();
                assert!(e.starts_with("automation path rejected:"), "{p}: {e}");
                assert!(e.contains(kind), "{p}: {e}");
                assert!(!e.contains("cannot resolve") && !e.to_ascii_lowercase().contains("os error"), "{p}: {e}");
            }
        }
        let e = roots.check(&s(base.join("outside/secret.svg")), Access::Read).unwrap_err();
        assert!(e.contains("outside the read root"), "a local path is still judged by its folder: {e}");
        let verbatim = format!(r"\\?\{}", s(base.join("in").join("a.svg")));
        assert_eq!(roots.check(&verbatim, Access::Read), Ok(()), "a verbatim disk path is a local path: {verbatim}");
        let _ = std::fs::remove_dir_all(base);
    }

    /// A root on a share lets that server and share through (either spelling, any ASCII case) and nothing else.
    #[cfg(windows)]
    #[test]
    fn a_root_on_a_share_lets_only_that_share_through() {
        let share = [Path::new(r"\\?\UNC\FileServer\Projects\work")];
        assert_eq!(refused_prefix(Path::new(r"\\fileserver\projects\a.svg"), &share), None);
        assert_eq!(refused_prefix(Path::new(r"\\?\UNC\FILESERVER\PROJECTS\a.svg"), &share), None);
        assert!(refused_prefix(Path::new(r"\\fileserver\secret\a.svg"), &share).is_some(), "another share");
        assert!(refused_prefix(Path::new(r"\\203.0.113.5\Projects\a.svg"), &share).is_some(), "another server");
        let work = [Path::new(r"\\?\UNC\work\Projects\w")];
        assert!(refused_prefix(Path::new("\\\\wor\u{212a}\\Projects\\a.svg"), &work).is_some(), "U+212A is not k");
        assert!(refused_prefix(Path::new(r"\\.\PhysicalDrive0"), &share).is_some(), "a share is not a device");
        let device = [Path::new(r"\\.\PhysicalDrive0")];
        assert_eq!(refused_prefix(Path::new(r"\\.\physicaldrive0"), &device), None, "the same device");
        assert!(refused_prefix(Path::new(r"\\.\PhysicalDrive1"), &device).is_some());
    }

    /// A link whose text is a bare `\\server` (no share) is refused as written: joined onto the link's folder
    /// it would read as a local path and pass.
    #[cfg(windows)]
    #[test]
    fn a_link_to_a_bare_server_name_is_refused() {
        let (base, roots) = folders("unc-link-bare");
        let link = base.join("in").join("bare");
        if std::os::windows::fs::symlink_dir(r"\\203.0.113.5", &link).is_err() {
            eprintln!("skipped: symbolic links can't be made here");
            let _ = std::fs::remove_dir_all(base);
            return;
        }
        let e = roots.check(&s(link.join("x.svg")), Access::Read).unwrap_err();
        assert!(e.contains("UNC"), "{e}");
        assert!(!e.contains("cannot resolve") && !e.to_ascii_lowercase().contains("os error"), "{e}");
        let _ = std::fs::remove_dir_all(base);
    }

    /// Nine links that each go through `sub\..\` to the next are nine links, not a reason to give up: both
    /// spellings of each target are judged once.
    #[cfg(windows)]
    #[test]
    fn a_chain_of_links_with_parent_names_is_followed_in_few_steps() {
        let (base, roots) = folders("link-chain");
        let folder = base.join("in");
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        for n in 1..=9 {
            let target = if n == 9 { "sub".to_string() } else { format!(r"sub\..\l{}", n + 1) };
            if std::os::windows::fs::symlink_dir(&target, folder.join(format!("l{n}"))).is_err() {
                eprintln!("skipped: symbolic links can't be made here");
                let _ = std::fs::remove_dir_all(base);
                return;
            }
        }
        assert_eq!(roots.check(&s(folder.join("l1").join("x.svg")), Access::Read), Ok(()));
        let _ = std::fs::remove_dir_all(base);
    }

    /// A link whose relative target goes through a missing folder and back (`missing\..\remote`) is
    /// judged with the `..` taken by name too, as Windows takes it: `remote` is a link to a network path.
    #[cfg(windows)]
    #[test]
    fn a_relative_link_through_a_missing_folder_still_reaches_the_network_check() {
        let (base, roots) = folders("unc-link-dotdot");
        let (remote, alias) = (base.join("in").join("remote"), base.join("in").join("alias"));
        if std::os::windows::fs::symlink_dir(r"\\203.0.113.5\s", &remote).is_err() {
            eprintln!("skipped: symbolic links can't be made here");
            let _ = std::fs::remove_dir_all(base);
            return;
        }
        assert!(std::os::windows::fs::symlink_dir(r"missing\..\remote", &alias).is_ok());
        let e = roots.check(&s(alias.join("x.svg")), Access::Read).unwrap_err();
        assert!(e.contains("UNC"), "{e}");
        assert!(!e.contains("cannot resolve") && !e.to_ascii_lowercase().contains("os error"), "{e}");
        let _ = std::fs::remove_dir_all(base);
    }

    /// A link whose text names a network path, or that has one as a folder on the way, is refused
    /// from that text before the link is followed: following it would contact the host.
    #[cfg(windows)]
    #[test]
    fn a_link_to_a_network_path_is_refused_before_it_is_followed() {
        let (base, roots) = folders("unc-link");
        let file_link = base.join("in/leak.svg");
        if std::os::windows::fs::symlink_file(r"\\203.0.113.5\s\x.svg", &file_link).is_err() {
            eprintln!("skipped: symbolic links can't be made here");
            let _ = std::fs::remove_dir_all(base);
            return;
        }
        let folder_link = base.join("in/folder-leak");
        assert!(std::os::windows::fs::symlink_dir(r"\\203.0.113.5\s", &folder_link).is_ok());
        for path in [file_link, folder_link.join("x.svg")] {
            let e = roots.check(&s(path.clone()), Access::Read).unwrap_err();
            assert!(e.contains("UNC"), "{}: {e}", path.display());
            assert!(!e.contains("cannot resolve") && !e.to_ascii_lowercase().contains("os error"), "{}: {e}", path.display());
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn scopes_nest_and_carry_to_other_threads() {
        let (base, roots) = folders("scopes");
        let outside = s(base.join("outside/secret.svg"));
        assert_eq!(check_read(&outside), Ok(()), "nothing is confined by default");
        confine(Some(&roots), || {
            assert!(check_read(&outside).is_err());
            assert_eq!(unconfined(|| check_read(&outside)), Ok(()));
            assert!(check_read(&outside).is_err(), "restored after unconfined");
            let handle = ScopeHandle::capture();
            let o = outside.clone();
            let there = std::thread::spawn(move || (check_read(&o).is_ok(), handle.run(|| check_read(&o).is_ok()))).join().unwrap();
            assert_eq!(there, (true, false), "another thread is confined only in the captured scope");
        });
        assert_eq!(check_read(&outside), Ok(()), "restored after confine");
        let r = std::panic::catch_unwind(|| confine(Some(&roots), || panic!("boom")));
        assert!(r.is_err());
        assert_eq!(check_read(&outside), Ok(()), "restored after a panic");
        assert_eq!(confine(None, || check_read(&outside)), Ok(()));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn device_names() {
        for d in ["CON", "nul", "Com1", "LPT9.txt", "aux.svg", "NUL ", "COM¹", "Lpt²", "lpt³.txt"] {
            assert!(is_device_name(d), "{d}");
        }
        for f in ["console", "com10", "COM0", "lpt", "a.svg", "nul-x", "COM⁴", "com¹0", "lpt¹x"] {
            assert!(!is_device_name(f), "{f}");
        }
    }

    /// #3604: on Windows `x.svg:evil` writes a hidden NTFS alternate data stream of `x.svg`, which
    /// stays inside the root yet shows in no folder listing, so a `:` in any component after the
    /// drive is refused (a drive letter's own colon is not a component).
    #[test]
    fn alternate_data_streams_are_refused() {
        for p in ["x.svg:evil", "out/x.svg:evil", "out/x.svg:evil:$DATA", "out/sub:evil/x.svg", "out/x.svg::$DATA", "out/:evil"] {
            assert_eq!(windows_name_refusal(p), Some("alternate data streams are not allowed"), "{p}");
        }
        for p in ["x.svg", "out/x.svg", "out/a b.svg", "out/x.svg.evil", "out/NUL"] {
            assert_ne!(windows_name_refusal(p), Some("alternate data streams are not allowed"), "{p}");
        }
        assert_eq!(windows_name_refusal("out/NUL"), Some("reserved device names are not allowed"));
        assert_eq!(windows_name_refusal("out/x.svg"), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_refuses_a_stream_inside_a_write_root() {
        let (base, roots) = folders("streams");
        let e = roots.check(&s(base.join("out").join("x.svg:evil")), Access::Write).unwrap_err();
        assert!(e.contains("alternate data streams"), "{e}");
        assert_eq!(roots.check(&s(base.join("out/x.svg")), Access::Write), Ok(()));
        let _ = std::fs::remove_dir_all(base);
    }
}
