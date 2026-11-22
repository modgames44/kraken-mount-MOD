//! Managed PKG installation on internal or an explicitly selected connected external drive, driven
//! from the web UI (browse a USB/extended drive, copy with progress, verify byte-for-byte, commit
//! atomically, with cancel). The installed PKG lands where Kraken Mount already scans it, so the
//! game then appears like any other. Ported from TITAN CORE (installer.rs, GPL-3.0-or-later),
//! adapted to Kraken Mount's paths and helpers; see CREDITS.md.
use crate::installer_io::{transfer, verify};
use serde_json::{json, Value};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::sync::{atomic::{AtomicBool, Ordering}, Mutex};
use std::{fs, io::{self, Write}, path::{Path, PathBuf}};

/// Internal install location: the system app dir, as `/user/app/<TID>/app.pkg`.
pub const ROOT: &str = "/user/app";
/// An older internal managed layout, still recognised by `roots()` for already-installed games.
pub const LEGACY_ROOT: &str = "/data/homebrew/kraken-mount/installed";
/// The receipt owner stamped into install.json, so Kraken Mount only ever touches its own installs.
const OWNER: &str = "KRAKEN MOUNT installer";
/// Internal storage kept free for the game cache a pkg needs at launch.
const RESERVE: u64 = 256 << 20;

struct Request {
    path: PathBuf,
    destination: String,
}

struct Destination {
    mount: PathBuf,
    root: PathBuf,
    external: bool,
    free: u64,
    dev: u64,
    ino: u64,
    base: PathBuf,
    base_dev: u64,
    base_ino: u64,
}

impl Destination {
    fn resolve(mount: &str) -> io::Result<Self> {
        let d = crate::storage::drives().into_iter().find(|d| d.mount == mount)
            .ok_or_else(|| io::Error::other("Destination drive is disconnected"))?;
        let p = PathBuf::from(&d.mount);
        if p.canonicalize()? != p {
            return Err(io::Error::other("Destination mount must not be a symlink"));
        }
        let m = fs::metadata(&p)?;
        let external = d.kind != "internal";
        let base = if external { p.clone() } else { PathBuf::from("/user") };
        if base.canonicalize()? != base {
            return Err(io::Error::other("Destination base must not be a symlink"));
        }
        let bm = fs::metadata(&base)?;
        Ok(Self {
            root: if external { p.join("KrakenMount/Games") } else { PathBuf::from(ROOT) },
            mount: p, external, free: d.free, dev: m.dev(), ino: m.ino(),
            base, base_dev: bm.dev(), base_ino: bm.ino(),
        })
    }
    fn check(&self) -> io::Result<()> {
        let m = fs::metadata(&self.mount)?;
        if m.dev() != self.dev || m.ino() != self.ino
            || fs::metadata(&self.base).map_or(true, |b| b.dev() != self.base_dev || b.ino() != self.base_ino) {
            return Err(io::Error::other("Destination drive changed or disconnected"));
        }
        Ok(())
    }
    fn create_root(&self) -> io::Result<()> {
        self.check()?;
        let tail = self.root.strip_prefix(&self.base).map_err(io::Error::other)?;
        let mut p = self.base.clone();
        for part in tail.components() {
            p.push(part);
            match fs::symlink_metadata(&p) {
                Ok(m) if !m.is_dir() || m.file_type().is_symlink() => return Err(io::Error::other("Installation folder must not contain symlinks")),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&p)?,
                Err(e) => return Err(e),
            }
            if p.canonicalize()? != p {
                return Err(io::Error::other("Installation folder changed"));
            }
        }
        self.check()
    }
}

/// The selectable install destinations for the UI: each connected drive, its free space, and where
/// a game would land on it.
pub fn destinations() -> Value {
    json!(crate::storage::drives().iter().map(|d| json!({
        "path": d.mount, "name": d.label, "free": d.free, "external": d.kind != "internal",
        "folder": if d.kind == "internal" { format!("{ROOT}/<TITLE_ID>/app.pkg") } else { format!("{}/KrakenMount/Games", d.mount) },
    })).collect::<Vec<_>>())
}

#[derive(Default)]
pub struct Installer {
    pub job: Mutex<Value>,
    pending: Mutex<Option<Request>>,
    cancel: AtomicBool,
}

impl Installer {
    pub fn status(&self) -> Value {
        self.job.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn active(&self) -> bool {
        matches!(self.status()["phase"].as_str(), Some("queued" | "copying" | "verifying"))
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
    pub fn queue(&self, path: PathBuf) -> io::Result<()> {
        self.queue_to(path, "/data")
    }
    pub fn queue_to(&self, path: PathBuf, destination: &str) -> io::Result<()> {
        Destination::resolve(destination)?;
        let mut job = self.job.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(job["phase"].as_str(), Some("queued" | "copying" | "verifying")) {
            return Err(io::Error::other("An installation is already active"));
        }
        self.cancel.store(false, Ordering::SeqCst);
        log!("installer: queued {} -> {destination}", path.display());
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(Request { path: path.clone(), destination: destination.into() });
        *job = json!({"phase": "queued", "path": path, "destination": destination, "done": 0, "total": 0});
        Ok(())
    }
    fn progress(&self, phase: &str, done: u64, total: u64) {
        let mut j = self.job.lock().unwrap_or_else(|e| e.into_inner());
        j["phase"] = json!(phase);
        j["done"] = json!(done);
        j["total"] = json!(total);
    }
    fn check(&self, state: &crate::web::State, dest: &Destination) -> io::Result<()> {
        if self.cancel.load(Ordering::SeqCst) || state.paused() {
            return Err(io::Error::other("Installation stopped; uncommitted source kept"));
        }
        dest.check()
    }
    /// Runs a queued installation to completion, if any. Called from the scan loop (never while a
    /// game runs). On any failure the destination is rolled back to exactly what was there before.
    pub fn run(&self, state: &crate::web::State) {
        let Some(request) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
        let path = request.path;
        let result = (|| -> io::Result<()> {
            let dest = Destination::resolve(&request.destination)?;
            self.check(state, &dest)?;
            let source = external(&path, false)?;
            let meta = fs::metadata(&source)?;
            let size = meta.len();
            let (tid, name, _) = crate::cache::peek_pkg(&source).map_err(io::Error::other)?;
            if !tid.starts_with("PPSA") || !crate::scan::valid_tid(&tid) {
                return Err(io::Error::other("Only supported PS5 debug PKGs can be installed"));
            }
            if dest.external && crate::storage::drives().iter().find(|d| d.mount == "/data").map_or(0, |d| d.free) < RESERVE {
                return Err(io::Error::other("Internal storage needs free space for the game cache"));
            }
            let needed = size.saturating_add(RESERVE);
            if dest.free < needed {
                return Err(io::Error::other("Not enough destination storage for PKG and working reserve"));
            }
            if state.games.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|g| g.src.title_id == tid && (g.src.drive == "/data" || (dest.external && g.src.path != source))) {
                return Err(io::Error::other("This title already has another library source; nothing was overwritten"));
            }
            dest.create_root()?;
            let dir = dest.root.join(&tid);
            let created_dir = match fs::create_dir(&dir) {
                Ok(()) => true,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && !dest.external => {
                    let m = fs::symlink_metadata(&dir)?;
                    let param = dir.join("sce_sys/param.json");
                    if !m.is_dir() || m.file_type().is_symlink() || dir.canonicalize()? != dir || param.canonicalize()? != param
                        || !metadata_title_matches(&param, &tid)
                        || crate::cache::pkg_source(&tid).as_deref() != source.to_str()
                        || dir.join("eboot.bin").exists() {
                        return Err(io::Error::other("Existing system title is not this Kraken source; nothing was overwritten"));
                    }
                    false
                }
                Err(e) => return Err(e),
            };
            for n in ["package.part", "install.json", "app.pkg"] {
                if fs::symlink_metadata(dir.join(n)).is_ok() {
                    if created_dir {
                        let _ = fs::remove_dir(&dir);
                    }
                    return Err(io::Error::other("System destination already contains package data; nothing was overwritten"));
                }
            }
            let owned = fs::metadata(&dir)?;
            let mut created_files: Vec<(PathBuf, u64, u64)> = Vec::new();
            let mut created_sys: Option<(u64, u64)> = None;
            let owned_dir = || fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink() && m.dev() == owned.dev() && m.ino() == owned.ino())
                && dir.canonicalize().ok().as_ref() == Some(&dir);
            let result = (|| -> io::Result<()> {
                let mode = owned.permissions().mode() & 0o7777;
                if !dest.external && created_dir {
                    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))?;
                } else if mode & 0o555 != 0o555 {
                    fs::set_permissions(&dir, fs::Permissions::from_mode(mode | 0o555))?;
                }
                let tmp = dir.join("package.part");
                let dst = dir.join(if dest.external { format!("{tid}.pkg") } else { "app.pkg".into() });
                let mut input = fs::File::open(&source)?;
                let mut out = fs::OpenOptions::new().write(true).create_new(true).mode(0o644).open(&tmp)?;
                let om = out.metadata()?;
                created_files.push((tmp.clone(), om.dev(), om.ino()));
                self.progress("copying", 0, size);
                transfer(&mut input, &mut out, size, |done| {
                    self.check(state, &dest)?;
                    if !owned_dir() {
                        return Err(io::Error::other("Destination folder changed"));
                    }
                    self.progress("copying", done, size);
                    Ok(())
                })?;
                out.sync_all()?;
                drop(out);
                self.progress("verifying", 0, size);
                verify(&source, &tmp, size, |done| {
                    self.check(state, &dest)?;
                    if !owned_dir() {
                        return Err(io::Error::other("Destination folder changed"));
                    }
                    self.progress("verifying", done, size);
                    Ok(())
                })?;
                let (read_tid, _, _) = crate::cache::peek_pkg(&tmp).map_err(io::Error::other)?;
                if read_tid != tid {
                    return Err(io::Error::other("Copied package title mismatch"));
                }
                let now = fs::metadata(&source)?;
                if now.dev() != meta.dev() || now.ino() != meta.ino() || now.modified()? != meta.modified()? || now.len() != size || source.canonicalize()? != source {
                    return Err(io::Error::other("Source package changed during installation"));
                }
                self.check(state, &dest)?;
                if !owned_dir() || dst.exists() {
                    return Err(io::Error::other("Destination changed; nothing was overwritten"));
                }
                // An internal install needs its own sce_sys so the system shows it on the home menu.
                if !dest.external && created_dir {
                    let param = crate::cache::pkg_file(&tmp, "sce_sys/param.json").map_err(io::Error::other)?;
                    let parsed: Value = serde_json::from_slice(&param).map_err(io::Error::other)?;
                    if parsed["titleId"].as_str().or_else(|| parsed["title_id"].as_str()) != Some(tid.as_str()) {
                        return Err(io::Error::other("System metadata title mismatch"));
                    }
                    let icon = crate::cache::pkg_file(&tmp, "sce_sys/icon0.png").map_err(io::Error::other)?;
                    let sys = dir.join("sce_sys");
                    fs::create_dir(&sys)?;
                    let sm = fs::metadata(&sys)?;
                    created_sys = Some((sm.dev(), sm.ino()));
                    fs::set_permissions(&sys, fs::Permissions::from_mode(0o755))?;
                    for (p, bytes) in [(sys.join("param.json"), param), (sys.join("icon0.png"), icon)] {
                        self.check(state, &dest)?;
                        if !owned_dir() || sys.canonicalize()? != sys {
                            return Err(io::Error::other("System metadata destination changed"));
                        }
                        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o644).open(&p)?;
                        let fm = f.metadata()?;
                        created_files.push((p, fm.dev(), fm.ino()));
                        f.write_all(&bytes)?;
                        f.sync_all()?;
                    }
                }
                let receipt = json!({"owner": OWNER, "title_id": tid, "size": size, "source": source, "destination": dest.mount, "operation": "copy", "original_mode": meta.permissions().mode() & 0o7777});
                let mut f = fs::OpenOptions::new().write(true).create_new(true).open(dir.join("install.json"))?;
                let fm = f.metadata()?;
                created_files.push((dir.join("install.json"), fm.dev(), fm.ino()));
                f.write_all(receipt.to_string().as_bytes())?;
                f.sync_all()?;
                drop(f);
                self.check(state, &dest)?;
                if !owned_dir() || dst.exists() {
                    return Err(io::Error::other("Destination changed; source kept"));
                }
                // Commit only the verified staging copy; the download original stays untouched.
                fs::rename(&tmp, &dst)?;
                log!("installer: installed {tid} ({name}) to {} ({size} bytes)", dst.display());
                *self.job.lock().unwrap_or_else(|e| e.into_inner()) = json!({"phase": "complete", "title_id": tid, "name": name, "path": dst, "destination": dest.mount, "operation": "copy", "done": size, "total": size});
                Ok(())
            })();
            if result.is_err() && dest.check().is_ok() && owned_dir() {
                for (p, dev, ino) in created_files.iter().rev() {
                    if p.canonicalize().ok().as_ref() == Some(p) && fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink() && m.dev() == *dev && m.ino() == *ino) {
                        let _ = fs::remove_file(p);
                    }
                }
                let sys = dir.join("sce_sys");
                if created_sys.is_some_and(|(dev, ino)| sys.canonicalize().ok().as_ref() == Some(&sys) && fs::symlink_metadata(&sys).is_ok_and(|m| m.is_dir() && m.dev() == dev && m.ino() == ino)) {
                    let _ = fs::remove_dir(sys);
                }
                if created_dir {
                    let _ = fs::remove_dir(&dir);
                }
            }
            result
        })();
        if let Err(e) = result {
            log!("PKG installation {}: {e}", path.display());
            let mut j = self.job.lock().unwrap_or_else(|e| e.into_inner());
            j["phase"] = json!("error");
            j["error"] = json!(e.to_string());
        }
        state.wake();
    }
}

/// Validates that `path` is a real file (or dir) on a connected external drive, no symlinks.
pub fn external(path: &Path, directory: bool) -> io::Result<PathBuf> {
    if path.components().any(|c| !matches!(c, std::path::Component::RootDir | std::path::Component::Normal(_))) {
        return Err(io::Error::other("Invalid external path"));
    }
    let canonical = path.canonicalize()?;
    if canonical != path {
        return Err(io::Error::other("External symlinks are not supported"));
    }
    let allowed = crate::storage::drives().iter().filter(|d| d.kind != "internal").any(|d| canonical.starts_with(&d.mount));
    if !allowed {
        return Err(io::Error::other("Select a connected USB or extended drive"));
    }
    let m = fs::metadata(&canonical)?;
    if directory && !m.is_dir() || !directory && (!m.is_file() || !canonical.extension().is_some_and(|x| x.eq_ignore_ascii_case("pkg"))) {
        return Err(io::Error::other("Select a PKG file or folder"));
    }
    Ok(canonical)
}

/// Lists external drives (empty path) or the pkgs and folders directly in an external directory,
/// for the UI's file browser.
pub fn browse(path: &str) -> io::Result<Value> {
    let drives = crate::storage::drives();
    let external_drives = drives.iter().filter(|d| d.kind != "internal")
        .map(|d| json!({"name": d.label, "path": d.mount, "directory": true})).collect::<Vec<_>>();
    if path.is_empty() {
        return Ok(json!({"path": "", "entries": external_drives}));
    }
    let dir = external(Path::new(path), true)?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir)?.take(1025) {
        let e = entry?;
        let m = e.file_type()?;
        if m.is_symlink() {
            continue;
        }
        let p = e.path();
        if m.is_dir() || p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pkg")) {
            entries.push(json!({"name": e.file_name().to_string_lossy(), "path": p, "directory": m.is_dir(), "size": if m.is_file() { e.metadata()?.len() } else { 0 }}));
        }
    }
    entries.sort_by_key(|e| (!e["directory"].as_bool().unwrap_or(false), e["name"].as_str().unwrap_or("").to_lowercase()));
    Ok(json!({"path": dir, "entries": entries, "limit": 1025}))
}

fn managed_roots() -> Vec<PathBuf> {
    let mut roots: Vec<_> = crate::storage::drives().iter()
        .map(|d| if d.kind == "internal" { PathBuf::from(ROOT) } else { Path::new(&d.mount).join("KrakenMount/Games") }).collect();
    roots.push(PathBuf::from(LEGACY_ROOT));
    roots
}

/// The Kraken-installed games' folders, validated by their install.json receipt. Added to the scan
/// roots so an installed game appears in the library like any scanned one.
pub fn roots() -> Vec<String> {
    let mut roots = Vec::new();
    for root in managed_roots() {
        if root.canonicalize().ok().as_ref() != Some(&root) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&root) else { continue };
        for e in entries.flatten() {
            let dir = e.path();
            let tid = e.file_name().to_string_lossy().into_owned();
            if !crate::scan::valid_tid(&tid) || dir.canonicalize().ok().as_ref() != Some(&dir) {
                continue;
            }
            let Ok(data) = fs::read(dir.join("install.json")) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&data) else { continue };
            let pkg = dir.join(if root == Path::new(ROOT) { "app.pkg".into() } else { format!("{tid}.pkg") });
            if v["owner"] == OWNER && v["title_id"] == tid.as_str() && pkg.canonicalize().ok().as_ref() == Some(&pkg)
                && fs::symlink_metadata(&pkg).is_ok_and(|m| m.is_file() && Some(m.len()) == v["size"].as_u64()) {
                roots.push(dir.to_string_lossy().into_owned());
            }
        }
    }
    roots.sort();
    roots
}

fn metadata_title_matches(path: &Path, tid: &str) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= 8 << 20)
        && fs::read(path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .is_some_and(|v| v["titleId"].as_str().or_else(|| v["title_id"].as_str()) == Some(tid))
}

/// The external managed install packages (`<ext>/KrakenMount/Games/<TID>/<TID>.pkg`), so the UI can
/// flag them; internal installs keep the system's own controls.
pub fn external_installs() -> Vec<PathBuf> {
    let bases: Vec<_> = crate::storage::drives().iter().filter(|d| d.kind != "internal")
        .map(|d| Path::new(&d.mount).join("KrakenMount/Games")).collect();
    roots().iter().filter_map(|root| {
        let dir = Path::new(root);
        if !bases.iter().any(|base| dir.parent() == Some(base.as_path())) {
            return None;
        }
        Some(dir.join(format!("{}.pkg", dir.file_name()?.to_str()?)))
    }).collect()
}
