//! Games installed on the console (/user/app, extended storage). See DESIGN.md "Installed games".

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One empty file per title id whose base install Kraken Mount handed to the installer and that
/// has not reported success yet: written before the hand-over, removed only on success.
pub const MARKERS: &str = "/data/krakenmount/installing";
/// Kraken Mount's own home-screen tile (register.rs TILE_ID): never a game.
const TILE_ID: &str = "KRKN00001";

/// An installed game.
#[derive(Debug, Clone)]
pub struct Game {
    pub title_id: String,
    pub title_name: String,
    pub version: String,
    /// /user/app/<TID> or /mnt/extN/user/app/<TID>
    pub path: PathBuf,
    /// app.pkg's size, or the ext dir's du
    pub size: u64,
    pub retail: bool,
}

/// The first 8 bytes of an installed app.pkg: `\x7FFIH` and a non-zero signed byte (offset 5;
/// debug packages carry 0x00, as the pkg reader's `header` and dump-forge test).
pub fn retail_header(head: &[u8]) -> bool {
    head.len() >= 6 && &head[..4] == b"\x7FFIH" && head[5] != 0
}

/// Every installed game: /user/app/<TID> with an app.pkg, then /mnt/ext0|1/user/app/<TID>.
pub fn list() -> Vec<Game> {
    list_in(Path::new("/"))
}

/// The first copy of `tid` under `root`: user/app/<TID>/app.pkg, mnt/ext0/user/app/<TID>,
/// mnt/ext1/user/app/<TID>. Only ENOENT/ENOTDIR is "absent": a stat error (a drive that answers
/// badly) counts as present, so register never treats a real install as its own.
pub fn copy_in(root: &Path, tid: &str) -> Option<PathBuf> {
    [root.join("user/app").join(tid).join("app.pkg"), root.join("mnt/ext0/user/app").join(tid),
        root.join("mnt/ext1/user/app").join(tid)]
        .into_iter()
        .find(|p| !matches!(fs::symlink_metadata(p),
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR))))
}

/// A copy exists and Kraken Mount has no unfinished install of it.
pub fn is_installed(tid: &str) -> bool {
    installed_in(Path::new("/"), tid)
}

pub fn installed_in(root: &Path, tid: &str) -> bool {
    copy_in(root, tid).is_some() && !marker_in(root, tid).exists()
}

/// The marker path for `tid` under `root`.
pub fn marker_in(root: &Path, tid: &str) -> PathBuf {
    root.join(MARKERS.trim_start_matches('/')).join(tid)
}

pub fn mark(tid: &str) -> io::Result<()> {
    let p = marker_in(Path::new("/"), tid);
    fs::create_dir_all(MARKERS)?;
    fs::write(&p, b"")
}

pub fn unmark(tid: &str) {
    let p = marker_in(Path::new("/"), tid);
    if let Err(e) = fs::remove_file(&p) {
        if e.kind() != io::ErrorKind::NotFound {
            log!("[installed] {tid}: cannot remove marker {}: {e}", p.display());
        }
    }
}

pub fn marked(tid: &str) -> bool {
    marker_in(Path::new("/"), tid).exists()
}

/// (name, version) from an installed game's appmeta param.json; None for a non-game
/// (`applicationCategoryType` set and not 0); Err for a file that does not parse.
pub fn parse_installed(json: &[u8]) -> io::Result<Option<(String, String)>> {
    let v: serde_json::Value = serde_json::from_slice(json)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("param.json: {e}")))?;
    let cat = &v["applicationCategoryType"];
    if !(cat.is_null() || cat.as_u64() == Some(0)) {
        return Ok(None);
    }
    let (_, name, version) = crate::scan::parse_param(json)?;
    Ok(Some((name, version)))
}

/// Title ids whose appmeta failed to parse, so each is logged once per run.
static LOGGED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// `list` under `root` (a fake tree in tests). One read_dir per base.
pub fn list_in(root: &Path) -> Vec<Game> {
    let mut out: Vec<Game> = Vec::new();
    let bases = [(root.join("user/app"), true), (root.join("mnt/ext0/user/app"), false),
        (root.join("mnt/ext1/user/app"), false)];
    for (base, internal) in bases {
        let Ok(rd) = fs::read_dir(&base) else { continue };
        for entry in rd.flatten() {
            let Some(tid) = entry.file_name().to_str().map(str::to_owned) else { continue };
            if !crate::scan::valid_tid(&tid) || tid == TILE_ID || out.iter().any(|g| g.title_id == tid)
                || marker_in(root, &tid).exists()
            {
                continue;
            }
            let dir = entry.path();
            let pkg = dir.join("app.pkg");
            let size = match fs::metadata(&pkg) {
                Ok(m) => m.len(),
                // No app.pkg in /user/app: Kraken's staging or an ext install's stub.
                Err(e) if internal && matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => continue,
                // Any other stat error counts as present, as in `copy_in`; the size is unknown.
                // ponytail: logged on every call while the error lasts.
                Err(e) if internal => {
                    log!("[installed] {tid}: cannot stat {}: {e}", pkg.display());
                    0
                }
                // ponytail: an ext copy without app.pkg is walked on every call.
                Err(_) => crate::cache::du(&dir),
            };
            let meta = root.join("user/appmeta").join(&tid).join("param.json");
            let parsed = fs::read(&meta).and_then(|j| parse_installed(&j));
            let (title_name, version) = match parsed {
                Ok(Some(nv)) => nv,
                Ok(None) => continue,
                // Still listed (rank 0), so an FPKG of the title never registers or mounts over the
                // install. ponytail: the title id stands in for the name, the version is unknown, and
                // a non-game app without a readable appmeta is listed as a game.
                Err(e) => {
                    let mut seen = LOGGED.lock().unwrap_or_else(|p| p.into_inner());
                    if seen.get_or_insert_with(HashSet::new).insert(tid.clone()) {
                        log!("[installed] {tid}: no name or version, {}: {e}", meta.display());
                    }
                    (tid.clone(), String::new())
                }
            };
            out.push(Game { title_id: tid, title_name, version, path: dir, size, retail: retail_of(&pkg) });
        }
    }
    out
}

/// `retail_header` of the first 8 bytes of `pkg`; false when it cannot be read.
pub fn retail_of(pkg: &Path) -> bool {
    let mut head = [0u8; 8];
    fs::File::open(pkg).and_then(|mut f| f.read_exact(&mut head)).is_ok() && retail_header(&head)
}
