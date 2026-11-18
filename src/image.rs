//! exFAT game images (.exfat, and .zstd holding an exFAT image) mounted read-only through
//! /dev/lvdctl, as ShadowMountPlus 1.7 does (sm_image.c attach_lvd_backend_as + perform_image_nmount).
//! Each image is mounted at IMAGE_BASE/<name>_<hash>; IMAGE_BASE is then scanned as one more
//! root, so the mounted image is an ordinary folder game (eboot.bin + sce_sys/param.json at its root).
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

pub const IMAGE_BASE: &str = "/mnt/kraken-img";

const LVD_CTRL: &str = "/dev/lvdctl";
const SCE_LVD_IOC_ATTACH_V0: libc::c_ulong = 0xC028_6D00;
const SCE_LVD_IOC_DETACH: libc::c_ulong = 0xC028_6D01;
/// SMP LVD_SECTOR_SIZE_EXFAT / LVD_SECONDARY_UNIT_IMAGE_IO.
const SECTOR_EXFAT: u32 = 512;
const SECONDARY_UNIT: u32 = 0x10000;
/// SMP LVD_ATTACH_IMAGE_TYPE_SINGLE (legacy 1.6) / _SAVE_DATA (optimized, source under /data or /user).
const IMAGE_TYPE_SINGLE: u16 = 0;
const IMAGE_TYPE_SAVE_DATA: u16 = 5;
/// normalize_lvd_raw_flags(LVD_ATTACH_RAW_FLAGS_SINGLE_RO): BASE 0x4 | RO_VARIANT 0x8 | SINGLE_SAVE 0x10.
const FLAGS_SINGLE_RO: u16 = 0x1C;
const ENTRY_TYPE_FILE: u16 = 1;
const ENTRY_TYPE_SPECIAL: u16 = 2;
const ENTRY_FLAG_NO_BITMAP: u16 = 1;
/// An image modified this recently may still be copying: mounted on a later scan.
const SETTLE: Duration = Duration::from_secs(15);
/// f_mntonname is 88 bytes on the FreeBSD 11 layout: IMAGE_BASE + "/" + name + "_" + 8 hex + NUL.
const MAX_NAME: usize = 88 - IMAGE_BASE.len() - 1 - 9 - 1;

#[repr(C)]
struct LvdLayer {
    source_type: u16,
    flags: u16,
    reserved0: u32,
    path: *const libc::c_char,
    offset: u64,
    size: u64,
    bitmap_path: *const libc::c_char,
    bitmap_offset: u64,
    bitmap_size: u64,
}

#[repr(C)]
struct LvdAttach {
    io_version: u32,
    device_id: i32,
    sector_size: u32,
    secondary_unit: u32,
    flags: u16,
    image_type: u16,
    layer_count: u32,
    device_size: u64,
    layers: *mut LvdLayer,
}

#[repr(C)]
struct LvdDetach {
    reserved0: u32,
    device_id: i32,
    reserved: [u8; 0x20],
}

const _: () = assert!(std::mem::size_of::<LvdAttach>() == 0x28);
const _: () = assert!(std::mem::size_of::<LvdDetach>() == 0x28);
const _: () = assert!(std::mem::size_of::<LvdLayer>() == 56);

/// What a candidate image holds, from its first sector.
#[derive(Debug, PartialEq, Eq)]
pub enum Content {
    Exfat,
    /// "PFSC"-style container or anything else: not mounted (no nested-image support).
    Other,
}

/// exFAT boot sector: "EXFAT   " at offset 3 (FileSystemName).
pub fn classify(head: &[u8]) -> Content {
    if head.len() >= 11 && &head[3..11] == b"EXFAT   " {
        Content::Exfat
    } else {
        Content::Other
    }
}

pub fn is_image_name(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exfat") || e.eq_ignore_ascii_case("zstd"))
}

/// IMAGE_BASE/<file stem cut to fit>_<fnv32 of the image path>.
pub fn mount_point(image: &Path) -> String {
    let stem = image.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let stem: String = stem.chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect();
    let mut cut = stem.len().min(MAX_NAME);
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    let h = image.as_os_str().as_encoded_bytes().iter()
        .fold(0x811c_9dc5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    format!("{IMAGE_BASE}/{}_{h:08x}", &stem[..cut])
}

struct Mounted {
    image: PathBuf,
    unit: Option<i32>,
}

/// Mount point -> image mounted there by this run (or found mounted from an earlier one).
static MOUNTS: OnceLock<Mutex<HashMap<String, Mounted>>> = OnceLock::new();
/// Last logged problem per image, so a bad image is not logged every scan.
static LOGGED: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();

fn mounts() -> std::sync::MutexGuard<'static, HashMap<String, Mounted>> {
    MOUNTS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|e| e.into_inner())
}

fn note(image: &Path, msg: String) {
    let mut logged = LOGGED.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|e| e.into_inner());
    if logged.get(image) != Some(&msg) {
        log!("{}: {msg}", image.display());
        logged.insert(image.to_path_buf(), msg);
    }
}

/// The image mounted at a scanned folder, so the UI shows the image's drive.
pub fn image_of(mount: &Path) -> Option<PathBuf> {
    mounts().get(mount.to_str()?).map(|m| m.image.clone())
}

struct Statfs {
    fstype: String,
    from: String,
    on: String,
}

fn statfs(path: &str) -> Option<Statfs> {
    let c = CString::new(path).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let s = |p: *const libc::c_char| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    Some(Statfs { fstype: s(st.f_fstypename.as_ptr()), from: s(st.f_mntfromname.as_ptr()), on: s(st.f_mntonname.as_ptr()) })
}

/// The lvd unit of an exFAT mount exactly at `mp` ("/dev/lvdN"), Some(None) when mounted from
/// something else, None when `mp` is not a mount point.
fn active(mp: &str) -> Option<Option<i32>> {
    let st = statfs(mp).filter(|st| st.on == mp)?;
    Some(st.from.strip_prefix("/dev/lvd").and_then(|n| n.parse().ok()).filter(|_| st.fstype == "exfatfs"))
}

fn head(image: &Path) -> io::Result<[u8; 512]> {
    let mut b = [0u8; 512];
    fs::File::open(image)?.read_exact(&mut b)?;
    Ok(b)
}

fn attach(image: &Path, size: u64) -> io::Result<i32> {
    let path = CString::new(image.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let special = fs::metadata(image).map(|m| {
        use std::os::unix::fs::FileTypeExt;
        m.file_type().is_block_device() || m.file_type().is_char_device()
    }).unwrap_or(false);
    let mut layer = LvdLayer {
        source_type: if special { ENTRY_TYPE_SPECIAL } else { ENTRY_TYPE_FILE },
        flags: ENTRY_FLAG_NO_BITMAP,
        reserved0: 0,
        path: path.as_ptr(),
        offset: 0,
        size,
        bitmap_path: std::ptr::null(),
        bitmap_offset: 0,
        bitmap_size: 0,
    };
    // SMP get_image_mount_profile: the optimized type-5 profile only for sources on internal storage.
    let s = image.to_string_lossy();
    let optimized = ["/data", "/user"].iter().any(|r| s == *r || s.starts_with(&format!("{r}/")));
    let mut req = LvdAttach {
        io_version: 0,
        device_id: -1,
        sector_size: SECTOR_EXFAT,
        secondary_unit: SECONDARY_UNIT,
        flags: FLAGS_SINGLE_RO,
        image_type: if optimized { IMAGE_TYPE_SAVE_DATA } else { IMAGE_TYPE_SINGLE },
        layer_count: 1,
        device_size: size,
        layers: &mut layer,
    };
    let ctl = CString::new(LVD_CTRL).unwrap();
    let fd = unsafe { libc::open(ctl.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return Err(io::Error::new(io::Error::last_os_error().kind(), format!("open {LVD_CTRL}: {}", io::Error::last_os_error())));
    }
    let rc = unsafe { libc::ioctl(fd, SCE_LVD_IOC_ATTACH_V0 as _, &mut req as *mut LvdAttach) };
    let e = io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if rc != 0 {
        return Err(io::Error::new(e.kind(), format!("lvd attach: {e}")));
    }
    if req.device_id < 0 {
        return Err(io::Error::other(format!("lvd attach returned unit {}", req.device_id)));
    }
    Ok(req.device_id)
}

fn detach(unit: i32) {
    let ctl = CString::new(LVD_CTRL).unwrap();
    let fd = unsafe { libc::open(ctl.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        log!("lvd{unit}: open {LVD_CTRL}: {}", io::Error::last_os_error());
        return;
    }
    let mut req = LvdDetach { reserved0: 0, device_id: unit, reserved: [0; 0x20] };
    if unsafe { libc::ioctl(fd, SCE_LVD_IOC_DETACH as _, &mut req as *mut LvdDetach) } != 0 {
        log!("lvd{unit}: detach: {}", io::Error::last_os_error());
    }
    unsafe { libc::close(fd) };
}

/// SMP wait_for_dev_node_state: up to 10 s for /dev/lvdN.
fn wait_node(dev: &str) -> bool {
    for _ in 0..100 {
        if Path::new(dev).exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// SMP's exFAT nmount profile, read-only: exfatfs + budgetid game + large + static timezone +
/// async + noatime + ignoreacl + automounted, with the kernel's errmsg.
fn nmount_exfat(dev: &str, mp: &str) -> io::Result<()> {
    let opts: [(&str, Option<&str>); 10] = [
        ("from", Some(dev)), ("fspath", Some(mp)), ("fstype", Some("exfatfs")),
        ("budgetid", Some("game")), ("large", Some("yes")), ("timezone", Some("static")),
        ("async", None), ("noatime", None), ("ignoreacl", None), ("automounted", None),
    ];
    let mut strings = Vec::with_capacity(opts.len() * 2);
    for (k, v) in opts {
        strings.push(Some(CString::new(k).unwrap()));
        strings.push(v.map(|v| CString::new(v)).transpose()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in mount option"))?);
    }
    let mut iov: Vec<libc::iovec> = strings.iter().map(|s| match s {
        Some(s) => libc::iovec { iov_base: s.as_ptr() as *mut libc::c_void, iov_len: s.as_bytes_with_nul().len() },
        None => libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 },
    }).collect();
    let errkey = CString::new("errmsg").unwrap();
    let mut errmsg = [0u8; 256];
    iov.push(libc::iovec { iov_base: errkey.as_ptr() as *mut libc::c_void, iov_len: errkey.as_bytes_with_nul().len() });
    iov.push(libc::iovec { iov_base: errmsg.as_mut_ptr() as *mut libc::c_void, iov_len: errmsg.len() });
    let rc = unsafe { libc::nmount(iov.as_mut_ptr(), iov.len() as libc::c_uint, libc::MNT_RDONLY as _) };
    if rc == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    let msg = CStr::from_bytes_until_nul(&errmsg).map(|m| m.to_string_lossy().into_owned()).unwrap_or_default();
    Err(io::Error::new(e.kind(), if msg.is_empty() { format!("nmount exfatfs: {e}") } else { format!("nmount exfatfs: {e} ({msg})") }))
}

fn mount(image: &Path, size: u64, mp: &str) -> io::Result<()> {
    fs::create_dir_all(mp)?;
    let unit = attach(image, size)?;
    let dev = format!("/dev/lvd{unit}");
    if !wait_node(&dev) {
        detach(unit);
        return Err(io::Error::new(io::ErrorKind::TimedOut, format!("{dev} did not appear")));
    }
    if let Err(e) = nmount_exfat(&dev, mp) {
        detach(unit);
        return Err(e);
    }
    // SMP verify_exfat_nmount_result: the mount at mp is exfatfs from our unit.
    if active(mp) != Some(Some(unit)) {
        let _ = crate::sys::unmount(mp, libc::MNT_FORCE as i32);
        detach(unit);
        return Err(io::Error::other(format!("{dev} did not mount as exfatfs at {mp}")));
    }
    log!("{}: mounted exFAT image {dev} -> {mp}", image.display());
    mounts().insert(mp.to_string(), Mounted { image: image.to_path_buf(), unit: Some(unit) });
    Ok(())
}

/// Drops a mount whose image left: its /system_ex/app/<TID> stack first (a nullfs over it keeps
/// it busy), then the exFAT mount (forced: the backing file may be gone), then the lvd unit.
fn release(mp: &str) {
    let unit = mounts().remove(mp).and_then(|m| m.unit).or_else(|| active(mp).flatten());
    if let Ok(json) = fs::read(format!("{mp}/sce_sys/param.json")) {
        if let Ok((tid, _, _)) = crate::scan::parse_param(&json) {
            if let Err(e) = crate::register::unmount_title(&tid) {
                log!("{tid}: unmount before image release: {e}");
            }
        }
    }
    if active(mp).is_some() {
        if let Err(e) = crate::sys::unmount(mp, libc::MNT_FORCE as i32) {
            log!("{mp}: unmount: {e}");
            return;
        }
    }
    if let Some(unit) = unit {
        detach(unit);
    }
    let _ = fs::remove_dir(mp);
    log!("{mp}: image released");
}

/// One pass over the scan roots: mounts new exFAT images (top-level .exfat / .zstd files, like
/// .pkg), and releases mounts whose image is gone. True when IMAGE_BASE holds mounted images,
/// i.e. it should be scanned as a root. Only called from the scan loop (no game running).
pub fn sync(roots: &[String]) -> bool {
    let mut wanted: HashMap<String, PathBuf> = HashMap::new();
    for root in roots {
        let Ok(rd) = fs::read_dir(root) else { continue };
        let mut paths: Vec<PathBuf> = rd.flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.path())
            .filter(|p| is_image_name(p))
            .collect();
        paths.sort();
        for image in paths {
            let Ok(meta) = fs::metadata(&image) else { continue };
            if !meta.is_file() {
                continue;
            }
            let mp = mount_point(&image);
            if wanted.contains_key(&mp) {
                continue;
            }
            wanted.insert(mp.clone(), image.clone());
            if let Some(m) = mounts().get(&mp) {
                if m.image == image && active(&mp).is_some() {
                    continue;
                }
            }
            // Mounted by an earlier run of the payload: adopt it.
            if let Some(unit) = active(&mp) {
                mounts().insert(mp.clone(), Mounted { image: image.clone(), unit });
                continue;
            }
            mounts().remove(&mp);
            let fresh = meta.modified().ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map_or(true, |age| age < SETTLE);
            if fresh {
                note(&image, "image changed recently; mounting it on a later scan".into());
                continue;
            }
            match head(&image).map(|h| classify(&h)) {
                Ok(Content::Exfat) => {}
                Ok(Content::Other) => {
                    note(&image, "not an exFAT image (a PFSC container or other format is not supported); skipped".into());
                    continue;
                }
                Err(e) => {
                    note(&image, format!("read: {e}"));
                    continue;
                }
            }
            if let Err(e) = mount(&image, meta.len(), &mp) {
                let _ = fs::remove_dir(&mp);
                note(&image, format!("mount failed: {e}"));
            }
        }
    }
    // Releases: our mounts whose image is not in this pass and no longer exists (a root that
    // failed to list, as a USB waking up, keeps its images), and leftovers from an earlier run.
    let mut gone: Vec<String> = mounts().iter()
        .filter(|(mp, m)| !wanted.contains_key(*mp) && !m.image.exists())
        .map(|(mp, _)| mp.clone())
        .collect();
    for e in fs::read_dir(IMAGE_BASE).into_iter().flatten().flatten() {
        let mp = format!("{IMAGE_BASE}/{}", e.file_name().to_string_lossy());
        if !wanted.contains_key(&mp) && !mounts().contains_key(&mp) && !gone.contains(&mp) {
            if active(&mp).is_some() {
                gone.push(mp);
            } else {
                let _ = fs::remove_dir(&mp);
            }
        }
    }
    for mp in gone {
        release(&mp);
    }
    !mounts().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_exfat_boot_sector() {
        let mut b = [0u8; 512];
        b[..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
        b[3..11].copy_from_slice(b"EXFAT   ");
        assert_eq!(classify(&b), Content::Exfat);
        assert_eq!(classify(b"PFSC\0\0\0\0\0\0\0\0"), Content::Other);
        assert_eq!(classify(&[0u8; 4]), Content::Other);
    }

    #[test]
    fn image_names() {
        assert!(is_image_name(Path::new("/data/homebrew/Game.exfat")));
        assert!(is_image_name(Path::new("/mnt/usb0/Game.ZSTD")));
        assert!(!is_image_name(Path::new("/mnt/usb0/Game.pkg")));
        assert!(!is_image_name(Path::new("/mnt/usb0/Game")));
    }

    #[test]
    fn mount_point_fits_mntonname() {
        let long = format!("/mnt/usb0/{}.exfat", "x".repeat(200));
        let mp = mount_point(Path::new(&long));
        assert!(mp.len() < 88, "{}", mp.len());
        assert!(mp.starts_with("/mnt/kraken-img/xxx"));
        let a = mount_point(Path::new("/mnt/usb0/G.exfat"));
        let b = mount_point(Path::new("/mnt/usb1/G.exfat"));
        assert_ne!(a, b);
        assert_eq!(a, mount_point(Path::new("/mnt/usb0/G.exfat")));
        // Multibyte names are cut on a char boundary.
        let mp = mount_point(Path::new(&format!("/data/{}.exfat", "é".repeat(100))));
        assert!(mp.len() < 88);
    }
}
