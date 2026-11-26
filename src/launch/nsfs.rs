//! NSFS redirects of a backport's loose files into an installed title's sandbox /app0. Port of
//! ShadowMountPlus 1.7 sm_pkg_backport.c: /dev/nsfsctl, a PLAIN redirect per file, an OVERLAY per
//! directory missing in app0, recursion into a directory app0 has; the backport's fakelib/fakelib2
//! at its root are left out (the common/lib overlay takes them). Up to 256 redirects per sandbox.
//! State follows the namespace identity (app0's st_dev + statfs fsid): deletes only against the
//! same namespace; a gone one released them. Kept in a static, so a watcher restart keeps them.
use super::{all_mounts, same_fsid, top_mount, SANDBOX_ROOT};
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const CONTROL: &str = "/dev/nsfsctl";
const IOCTL_CREATE: libc::c_ulong = 0xC040_6E00;
const IOCTL_DELETE: libc::c_ulong = 0xC040_6E01;
/// SMP NSFS_SCOPE_UNCONDITIONAL, NSFS_BUDGET_BIGAPP.
const SCOPE_UNCONDITIONAL: u32 = 0;
const BUDGET_BIGAPP: i32 = 0;
/// SMP PKG_BACKPORT_MAX_REDIRECTS / PKG_BACKPORT_MAX_DIRECTORY_DEPTH.
pub const MAX_REDIRECTS: usize = 256;
const MAX_DEPTH: usize = 64;

/// SMP nsfs_redirect_desc_t.
#[repr(C)]
struct CreateDesc {
    source: *const libc::c_char,
    destination: *const libc::c_char,
    aux0: u64,
    kind: u32,
    scope: u32,
    budget_id: i32,
    reserved0: u32,
    opaque: [u8; 0x18],
}

/// SMP nsfs_delete_redirect_t.
#[repr(C)]
struct DeleteDesc {
    reserved0: u64,
    destination: *const libc::c_char,
    zero: [u8; 0x30],
}

const _: () = assert!(std::mem::size_of::<CreateDesc>() == 0x40);
const _: () = assert!(std::mem::size_of::<DeleteDesc>() == 0x40);

/// SMP NSFS_REDIRECT_OVERLAY (1) / NSFS_REDIRECT_PLAIN (2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Overlay = 1,
    Plain = 2,
}

#[derive(Debug)]
pub struct Redirect {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub kind: Kind,
}

fn os_err(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

/// SMP create_child_redirects_locked as a plan: every entry of `src` (readdir order) against `dst`.
/// A regular file -> PLAIN; a directory -> recurse when `dst` has it as a directory, OVERLAY when
/// `dst` lacks it (ENOENT), ENOTDIR when `dst` has a non-directory; anything else (symlink) skipped.
/// Only "fakelib"/"fakelib2" at the root are excluded (dot files are not). Past MAX_REDIRECTS: E2BIG,
/// past MAX_DEPTH: ELOOP; any failure fails the whole plan (SMP rolls back every redirect).
pub fn plan(src: &Path, dst: &Path) -> io::Result<Vec<Redirect>> {
    let mut out = Vec::new();
    walk(src, dst, true, 0, &mut out)?;
    Ok(out)
}

fn walk(src: &Path, dst: &Path, root: bool, depth: usize, out: &mut Vec<Redirect>) -> io::Result<()> {
    if depth >= MAX_DEPTH {
        return Err(os_err(libc::ELOOP));
    }
    for e in fs::read_dir(src)? {
        let name = e?.file_name();
        if root && (name == "fakelib" || name == "fakelib2") {
            continue;
        }
        let (s, d) = (src.join(&name), dst.join(&name));
        let ft = fs::symlink_metadata(&s)?.file_type();
        let kind = if ft.is_file() {
            Kind::Plain
        } else if !ft.is_dir() {
            continue;
        } else {
            match fs::metadata(&d) {
                Ok(m) if m.is_dir() => {
                    walk(&s, &d, false, depth + 1, out)?;
                    continue;
                }
                Ok(_) => return Err(os_err(libc::ENOTDIR)),
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Kind::Overlay,
                Err(e) => return Err(e),
            }
        };
        if out.len() >= MAX_REDIRECTS {
            return Err(os_err(libc::E2BIG));
        }
        out.push(Redirect { src: s, dst: d, kind });
    }
    Ok(())
}

/// The system's own libSceAmpr.sprx: what an installed game loads in place of its embedded copy.
const SYSTEM_AMPR: &str = "/system/common/lib/libSceAmpr.sprx";
const AMPR_LIB: &str = "libSceAmpr.sprx";
/// Where an install can carry its own libSceAmpr.sprx in /app0.
const AMPR_DIRS: [&str; 3] = ["sce_module", "fakelib", "fakelib2"];

/// ampr_installed off (no ng): the backport's own libSceAmpr.sprx is not redirected, and every
/// libSceAmpr.sprx (any case) the install has in app0's AMPR_DIRS gets a PLAIN redirect to `system`,
/// so the game runs the system's. Nothing when `system` is missing. ponytail: a backport dir
/// redirected whole (OVERLAY, absent in app0) keeps a libSceAmpr.sprx inside it.
pub fn hide_ampr_plan(plan: &mut Vec<Redirect>, app0: &Path, system: &Path) {
    let is_ampr = |p: &Path| p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(AMPR_LIB));
    plan.retain(|r| !(r.kind == Kind::Plain && is_ampr(&r.dst)));
    if !system.is_file() {
        return;
    }
    for d in AMPR_DIRS {
        let Ok(rd) = fs::read_dir(app0.join(d)) else { continue };
        for e in rd.flatten() {
            let dst = e.path();
            if is_ampr(&dst) && fs::symlink_metadata(&dst).is_ok_and(|m| m.is_file()) {
                plan.push(Redirect { src: system.to_path_buf(), dst, kind: Kind::Plain });
            }
        }
    }
}

/// SMP pkg_backport_namespace_id_t: app0's st_dev and statfs fsid.
struct Ns {
    dev: u64,
    fsid: libc::fsid_t,
}

impl Ns {
    fn same(&self, o: &Ns) -> bool {
        self.dev == o.dev && same_fsid(&self.fsid, &o.fsid)
    }
}

/// The redirects made in one sandbox's app0 namespace (destinations, oldest first).
struct Active {
    tid: String,
    /// The redirected backport dir (SMP sm_pkg_backport_source_is_in_use); None: only the ampr hide.
    root: Option<PathBuf>,
    app0: String,
    ns: Ns,
    dsts: Vec<CString>,
}

/// Keyed by sandbox dir name ("PPSA00000_000").
static ACTIVE: Mutex<Vec<(String, Active)>> = Mutex::new(Vec::new());

/// `path` is a directory with an nsfs mounted right on it: its namespace identity.
/// Err for a stat/statfs failure (ENOENT/ENOTDIR when it is not there), Ok(None) when not nsfs.
fn ns_at(path: &str) -> io::Result<Option<Ns>> {
    let m = fs::metadata(path)?;
    if !m.is_dir() {
        return Ok(None);
    }
    let t = top_mount(path)?;
    Ok((t.fstype == "nsfs" && t.on == path).then(|| Ns { dev: m.dev(), fsid: t.fsid }))
}

fn gone_errno(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR))
}

/// SMP resolve_pkg_app0_path: the one /mnt/sandbox/<tid>_<digits>/app0 that is an nsfs mount.
/// Two: EBUSY; none: ENOENT (a quiet skip).
fn resolve_app0(tid: &str) -> io::Result<(String, Ns)> {
    let mut found = None;
    for e in fs::read_dir(SANDBOX_ROOT)? {
        let name = e?.file_name().to_string_lossy().into_owned();
        let Some(n) = name.strip_prefix(tid).and_then(|r| r.strip_prefix('_')) else { continue };
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let app0 = format!("{SANDBOX_ROOT}/{name}/app0");
        match ns_at(&app0) {
            Ok(Some(_)) if found.is_some() => return Err(os_err(libc::EBUSY)),
            Ok(Some(ns)) => found = Some((app0, ns)),
            Ok(None) => {}
            Err(e) if gone_errno(&e) => {}
            Err(e) => return Err(e),
        }
    }
    found.ok_or_else(|| os_err(libc::ENOENT))
}

/// SMP session_namespace_status: Ok(true) the recorded namespace is still at app0, Ok(false) it is
/// gone, Err when it cannot be verified (EBUSY: still mounted elsewhere, covered).
fn status(a: &Active) -> io::Result<bool> {
    match ns_at(&a.app0) {
        Ok(Some(ns)) if ns.same(&a.ns) => return Ok(true),
        Ok(_) => {}
        Err(e) if gone_errno(&e) => {}
        Err(e) => return Err(e),
    }
    // all_mounts logs and returns empty when getmntinfo fails: unknown, never proof of teardown (SMP).
    let mounts = all_mounts();
    if mounts.is_empty() {
        return Err(os_err(libc::EIO));
    }
    if mounts.iter().any(|m| m.fstype == "nsfs" && same_fsid(&m.fsid, &a.ns.fsid)) {
        return Err(os_err(libc::EBUSY));
    }
    Ok(false)
}

fn control() -> io::Result<fs::File> {
    fs::OpenOptions::new().read(true).write(true).open(CONTROL)
}

fn create(fd: i32, r: &Redirect) -> io::Result<CString> {
    let src = CString::new(r.src.as_os_str().as_bytes()).map_err(|_| os_err(libc::EINVAL))?;
    let dst = CString::new(r.dst.as_os_str().as_bytes()).map_err(|_| os_err(libc::EINVAL))?;
    let mut d = CreateDesc {
        source: src.as_ptr(), destination: dst.as_ptr(), aux0: 0, kind: r.kind as u32,
        scope: SCOPE_UNCONDITIONAL, budget_id: BUDGET_BIGAPP, reserved0: 0, opaque: [0; 0x18],
    };
    if unsafe { libc::ioctl(fd, IOCTL_CREATE, &mut d as *mut CreateDesc) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(dst)
}

/// SMP delete_redirect: ENOENT/EINVAL count as deleted.
fn delete(fd: i32, dst: &CString) -> io::Result<()> {
    let mut d = DeleteDesc { reserved0: 0, destination: dst.as_ptr(), zero: [0; 0x30] };
    if unsafe { libc::ioctl(fd, IOCTL_DELETE, &mut d as *mut DeleteDesc) } == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOENT) | Some(libc::EINVAL) => Ok(()),
        _ => Err(e),
    }
}

/// Deletes `a`'s redirects newest first; the ones that stay are kept in `a`. The first error, or a
/// later non-EBUSY one after an EBUSY (SMP cleanup_session_locked: such a session is retained at once).
fn delete_all(a: &mut Active, fd: i32) -> io::Result<()> {
    let mut first = None;
    let mut kept = Vec::new();
    while let Some(d) = a.dsts.pop() {
        if let Err(e) = delete(fd, &d) {
            tlog!("{} backport redirect delete FAILED errno={} ({e}): {}", a.tid, e.raw_os_error().unwrap_or(0), d.to_string_lossy());
            if first.as_ref().map_or(true, |f: &io::Error| f.raw_os_error() == Some(libc::EBUSY)) {
                first = Some(e);
            }
            kept.push(d);
        }
    }
    kept.reverse();
    a.dsts = kept;
    first.map_or(Ok(()), Err)
}

pub enum Applied {
    /// The sandbox's /app0 is not an nsfs mount (yet, or never: a quiet skip).
    NotNsfs,
    /// This many redirects are in place (0: nothing in the backport outside fakelib/fakelib2).
    Done(usize),
}

/// SMP sm_pkg_backport_on_sandbox_ready for sandbox `sandbox` of `tid`: redirects `backport` into its
/// nsfs /app0. Again in the same namespace: Done with the count made before. A failed create rolls
/// back every redirect made (SMP); one that cannot be deleted stays for release().
pub fn apply(tid: &str, sandbox: &str, backport: Option<&Path>, hide_ampr: bool) -> io::Result<Applied> {
    let own = format!("{SANDBOX_ROOT}/{sandbox}/app0");
    // Cheap first: called every 1 ms while the sandbox comes up.
    match ns_at(&own) {
        Ok(Some(_)) => {}
        Ok(None) => return Ok(Applied::NotNsfs),
        Err(e) if gone_errno(&e) => return Ok(Applied::NotNsfs),
        Err(e) => return Err(e),
    }
    let mut active = ACTIVE.lock().unwrap();
    let mut same = None;
    // A new namespace does not prove the old one was destroyed (SMP): drop only verified-gone ones.
    active.retain(|(n, a)| {
        if n != sandbox {
            return true;
        }
        match status(a) {
            Ok(true) => {
                same = Some(a.dsts.len());
                true
            }
            Ok(false) => false,
            Err(_) => true,
        }
    });
    if let Some(n) = same {
        return Ok(Applied::Done(n));
    }
    let (app0, ns) = match resolve_app0(tid) {
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(Applied::NotNsfs),
        r => r?,
    };
    if app0 != own {
        return Err(os_err(libc::EBUSY));
    }
    let mut plan = match backport {
        Some(b) => plan(b, Path::new(&app0))?,
        None => Vec::new(),
    };
    if hide_ampr {
        hide_ampr_plan(&mut plan, Path::new(&app0), Path::new(SYSTEM_AMPR));
        if plan.len() > MAX_REDIRECTS {
            return Err(os_err(libc::E2BIG));
        }
    }
    if plan.is_empty() {
        return Ok(Applied::Done(0));
    }
    let ctl = control()?;
    let fd = ctl.as_raw_fd();
    let mut a = Active { tid: tid.to_string(), root: backport.map(Path::to_path_buf), app0, ns, dsts: Vec::new() };
    // Resolved before nsfsctl was opened: never into a namespace replaced in the meantime.
    if !status(&a)? {
        return Ok(Applied::NotNsfs);
    }
    for r in &plan {
        match create(fd, r) {
            Ok(d) => a.dsts.push(d),
            Err(e) => {
                tlog!("{tid} backport redirect FAILED errno={} ({e}): {} -> {} ({:?}); rolling back {}",
                    e.raw_os_error().unwrap_or(0), r.src.display(), r.dst.display(), r.kind, a.dsts.len());
                if matches!(status(&a), Ok(true)) {
                    let _ = delete_all(&mut a, fd);
                }
                if !a.dsts.is_empty() {
                    active.push((sandbox.to_string(), a));
                }
                return Err(e);
            }
        }
    }
    let n = a.dsts.len();
    active.push((sandbox.to_string(), a));
    Ok(Applied::Done(n))
}

/// SMP sm_pkg_backport_source_is_in_use: a recorded (live or retained) redirect reads from `path`
/// or below it, so it must not be deleted. A record whose namespace is verified gone is dropped
/// first (SMP prepare_session_locked reclaims those): one a failed release() retained (EIO from
/// getmntinfo) or one whose sandbox went while no watcher ran never blocks the delete for good.
pub fn in_use(path: &Path) -> bool {
    let reads = |a: &Active| a.root.as_ref().is_some_and(|r| r.starts_with(path) || path.starts_with(r));
    let mut active = ACTIVE.lock().unwrap();
    active.retain(|(_, a)| {
        if !reads(a) || !matches!(status(a), Ok(false)) {
            return true;
        }
        tlog!("{} backport redirects released with the namespace: {} in {}", a.tid, a.dsts.len(), a.app0);
        false
    });
    active.iter().any(|(_, a)| reads(a))
}

/// Redirects of ours are recorded for `sandbox`.
pub fn has(sandbox: &str) -> bool {
    ACTIVE.lock().unwrap().iter().any(|(n, _)| n == sandbox)
}

/// SMP cleanup_session_locked for `sandbox`: deletes its redirects only while their namespace is
/// still at app0; a gone namespace released them. Some(EBUSY) while it cannot be verified or a
/// delete is busy (the caller retries); other errors keep them (SMP: retained), logged, None.
pub fn release(sandbox: &str) -> Option<i32> {
    let mut busy = None;
    ACTIVE.lock().unwrap().retain_mut(|(n, a)| {
        if n != sandbox {
            return true;
        }
        let count = a.dsts.len();
        let r = match status(a) {
            Ok(false) => {
                tlog!("{} backport redirects released with the namespace: {count} in {}", a.tid, a.app0);
                return false;
            }
            Ok(true) => control().and_then(|c| delete_all(a, c.as_raw_fd())),
            Err(e) => Err(e),
        };
        match r {
            Ok(()) => {
                tlog!("{} backport redirects REMOVED: {count} in {}", a.tid, a.app0);
                false
            }
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                busy = Some(libc::EBUSY);
                true
            }
            Err(e) => {
                tlog!("{} backport redirects retained ({e}): {} in {}", a.tid, a.dsts.len(), a.app0);
                true
            }
        }
    });
    busy
}
