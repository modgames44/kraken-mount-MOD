/* Package install through the console's installer: a port of ps5-dump-forge ps5/installer.c
 * (forge_pkg_install) plus the status and cancel calls, with ShadowMountPlus's ABI
 * (include/sm_pkg_native.h). sceAppInstUtilInstallByPackage gets an http:// URL that
 * src/install.rs serves on 127.0.0.1; Sony accepts or refuses at once, then fetches the package
 * over HTTP Range requests on its own threads. Nothing is printed here: each call returns its
 * native codes, and km_pkg_install a short note, for the caller (src/, Rust) to log.
 */
#include <ps5/kernel.h>

#include <pthread.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

void km_appinst_begin(void);
void km_appinst_end(void);
int km_pkg_install(const char *url, const char *name, char content_id[48], int *task_type,
                   unsigned *code, char *note, size_t note_len);
int km_pkg_status(const char *content_id, int patch, char status[16], uint64_t *downloaded,
                  uint64_t *total, int *error_code);
int km_addcont_installed(const char *content_id, int *state);
int km_pkg_cancel(const char *content_id);

/* BGFT's gate for an http:// install and for the library's initialization: the SYSTEM authid. */
#define SYSTEM_INSTALL_AUTHID 0x4801000000000013ULL
/* ShellCore's: what InstallByPackage wants on FW < 11, where the SYSTEM one gets 0x80B22404 (Sony's
 * pre-flight of the URL refused, no byte fetched; ps5upload's authid.h, etaHEN's pkg write-up). */
#define SHELLCORE_AUTHID 0x3800000000000010ULL
#define REFUSED_PREFLIGHT 0x80B22404u
#define MAX_INSTALL_URL 127
/* SCE kernel EINVAL, used only if the caller violates the URL limit. */
#define BAD_INSTALL_URL 0x80020016u

/* SMP's AppInstUtil layouts, sizes verified against the console's 9.40 library. The metadata is
 * six pointers and eight words: Sony reads the word after icon (nonzero there sends a plain install
 * down the patch path, 0x80B2116F), so the whole struct is zeroed first. */
typedef struct { char content_id[48]; int type, platform; } pkg_info;
typedef struct {
    int32_t code, version;
    char description[512], type[9];
} pkg_error;
typedef struct {
    char status[16], source[8];
    float remaining_seconds;
    uint64_t downloaded, initial_chunk, total;
    uint32_t promotion;
    unsigned char reserved[100];
    pkg_error error;
    char patch_version[12];
    int32_t local_copy_percent;
    bool copy_only;
} pkg_status;
typedef struct {
    const char *uri, *ex_uri, *scenario, *content_id, *name, *icon;
    uint64_t reserved[8];
} pkg_metadata;
typedef struct {
    char languages[30][8], scenarios[64][3], content_ids[64][48];
    unsigned char reserved[6488];
} pkg_playgo;
_Static_assert(sizeof(pkg_metadata) == 0x70, "AppInstUtil metadata ABI");
_Static_assert(sizeof(pkg_info) == 0x38, "AppInstUtil info ABI");
_Static_assert(sizeof(pkg_playgo) == 0x2708, "AppInstUtil PlayGo ABI");
_Static_assert(sizeof(pkg_status) == 0x2c8, "AppInstUtil status ABI");
_Static_assert(offsetof(pkg_status, downloaded) == 0x20, "AppInstUtil download offset");
_Static_assert(offsetof(pkg_status, error) == 0xa0, "AppInstUtil error offset");
_Static_assert(offsetof(pkg_status, local_copy_percent) == 0x2c0, "AppInstUtil copy offset");

int sceAppInstUtilInitialize(void);
int sceAppInstUtilInstallByPackage(pkg_metadata *metadata, pkg_info *info, pkg_playgo *playgo);
int sceAppInstUtilGetInstallStatus(const char *content_id, pkg_status *status);
int sceAppInstUtilGetPatchInstallStatus(const char *content_id, pkg_status *status);
/* Writes one 32-bit state (4 = installed), not a status struct. Verified on firmware 9.40. */
int sceAppInstUtilGetAddcontInstalledStatus(const char *content_id, int *installed);
int sceAppInstUtilCancelInstall(const char *content_id);

/* Every sceAppInstUtil call in the payload runs under this lock, one at a time: the calls here
 * take it themselves, src/register.rs takes it through km_appinst_begin/end (src/install.rs
 * AppInst). Not recursive: a holder never calls back in. */
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static int initialized;

void km_appinst_begin(void) { pthread_mutex_lock(&lock); }

void km_appinst_end(void) { pthread_mutex_unlock(&lock); }

/* Static, not on the thread's stack: the PlayGo info is about 10 KiB. Used under `lock`. */
static pkg_metadata meta;
static pkg_info info;
static pkg_playgo playgo;
static pkg_status status_buf;

/* Appends one formatted line to the caller's note (truncated when full). */
__attribute__((format(printf, 3, 4)))
static void note_add(char *note, size_t len, const char *fmt, ...) {
    size_t used = strnlen(note, len);
    if (used + 1 >= len) return;
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(note + used, len - used, fmt, ap);
    va_end(ap);
}

/* This process's credentials as the installer needs them, and what they were before. */
typedef struct {
    uint64_t authid;
    uint8_t caps[16];
    uint8_t attrs[32];
    int have_caps, have_attrs;
} creds;

/* Takes on ps5upload's installer credentials (its escalate.c): the SYSTEM authid, every
 * capability, attrs 0x80. With the authid alone Sony refused at once (0x80B22404, on a dump-forge
 * user's console). Returns what to restore. */
static creds creds_acquire(char *note, size_t len) {
    pid_t pid = getpid();
    creds saved;
    memset(&saved, 0, sizeof(saved));
    saved.authid = kernel_get_ucred_authid(pid);
    saved.have_caps = kernel_get_ucred_caps(pid, saved.caps) == 0;
    saved.have_attrs = kernel_get_ucred_attrs(pid, saved.attrs) == 0;
    /* Swapped only when the old value was read (0: the read failed), so creds_release can put it
     * back: never left at SYSTEM for the rest of the run. */
    if (saved.authid == 0) note_add(note, len, "can't read the authid (not swapped); ");
    else if (kernel_set_ucred_authid(pid, SYSTEM_INSTALL_AUTHID) != 0)
        note_add(note, len, "can't swap the authid; ");
    /* Raised only when the old value was read, so creds_release can put it back: never left at
     * 0xff for the rest of the run. */
    uint8_t caps[16];
    memset(caps, 0xff, sizeof(caps));
    if (!saved.have_caps) note_add(note, len, "can't read the capabilities (not raised); ");
    else if (kernel_set_ucred_caps(pid, caps) != 0) note_add(note, len, "can't raise the capabilities; ");
    uint8_t attrs[32];
    memset(attrs, 0, sizeof(attrs));
    attrs[0] = 0x80;
    if (!saved.have_attrs) note_add(note, len, "can't read the attrs (not set); ");
    else if (kernel_set_ucred_attrs(pid, attrs) != 0) note_add(note, len, "can't set the attrs; ");
    return saved;
}

static void creds_release(const creds *saved, char *note, size_t len) {
    pid_t pid = getpid();
    if (saved->have_caps) (void)kernel_set_ucred_caps(pid, saved->caps);
    if (saved->have_attrs) (void)kernel_set_ucred_attrs(pid, saved->attrs);
    if (saved->authid == 0) return;
    for (int i = 0; i < 3; i++) {
        (void)kernel_set_ucred_authid(pid, saved->authid);
        if (kernel_get_ucred_authid(pid) == saved->authid) return;
    }
    note_add(note, len, "can't restore the authid 0x%llx; ", (unsigned long long)saved->authid);
}

/* Hands `url` (at most 127 bytes) to InstallByPackage. 0 when Sony accepts it, with the content
 * id and task type (0 base, 1 addcont, 2 patch) Sony filled in; else -1 with its error in *code.
 * `note` gets the native return codes and credential problems, one "...; " entry each. */
int km_pkg_install(const char *url, const char *name, char content_id[48], int *task_type,
                   unsigned *code, char *note, size_t note_len) {
    *code = 0;
    *task_type = -1;
    memset(content_id, 0, 48);
    if (note_len) note[0] = 0;
    if (!url || !*url || strlen(url) > MAX_INSTALL_URL) {
        *code = BAD_INSTALL_URL;
        return -1;
    }
    pthread_mutex_lock(&lock);
    creds saved = creds_acquire(note, note_len);
    /* ponytail: initialized once and never terminated, as ps5upload does (after the credentials:
     * Initialize runs as SYSTEM). Only a success is kept: a failed Initialize is tried again by the
     * next install. src/register.rs's own Initialize (as the payload's authid) is separate. */
    int rc = 0;
    if (!initialized) {
        rc = sceAppInstUtilInitialize();
        if (rc == 0)
            initialized = 1;
        else
            note_add(note, note_len, "sceAppInstUtilInitialize: 0x%08x; ", (unsigned)rc);
    }
    if (rc == 0) {
        memset(&meta, 0, sizeof(meta));
        memset(&info, 0, sizeof(info));
        memset(&playgo, 0, sizeof(playgo));
        meta.uri = url;
        meta.ex_uri = "";
        meta.scenario = "";
        meta.content_id = "";
        meta.name = name ? name : "";
        meta.icon = "";
        rc = sceAppInstUtilInstallByPackage(&meta, &info, &playgo);
        note_add(note, note_len, "InstallByPackage: 0x%08x; ", (unsigned)rc);
        /* ponytail: asked again as ShellCore rather than reading the firmware version first. */
        if ((unsigned)rc == REFUSED_PREFLIGHT && saved.authid != 0 &&
            kernel_set_ucred_authid(getpid(), SHELLCORE_AUTHID) == 0) {
            /* The refused call may have written them: Sony reacts to stray nonzero words. */
            memset(&info, 0, sizeof(info));
            memset(&playgo, 0, sizeof(playgo));
            rc = sceAppInstUtilInstallByPackage(&meta, &info, &playgo);
            note_add(note, note_len, "as ShellCore: 0x%08x; ", (unsigned)rc);
        }
        if (rc == 0) {
            memcpy(content_id, info.content_id, 47);
            *task_type = info.type;
        }
    }
    creds_release(&saved, note, note_len);
    pthread_mutex_unlock(&lock);
    if (rc == 0) return 0;
    *code = (unsigned)rc;
    return -1;
}

/* The calls below run as the installer's credentials too (SMP runs every AppInstUtil call with one
 * fixed elevated authid). ponytail: a credential problem here is not reported (the native rc
 * is); km_pkg_install's note shows the same swap failing. */

/* The once-only Initialize of km_pkg_install, for a status or DLC check that runs before the first
 * install (SMP initializes before any call). Under `lock`, with the credentials swapped. A failure
 * is not reported here (the call's own rc is) and is tried again next time. */
static void init_once(void) {
    if (!initialized && sceAppInstUtilInitialize() == 0) initialized = 1;
}

/* GetPatchInstallStatus when `patch`, else GetInstallStatus. Returns the native rc; the outputs
 * are filled only when it is 0 (status NUL-terminated, at most 15 chars). */
int km_pkg_status(const char *content_id, int patch, char status[16], uint64_t *downloaded,
                  uint64_t *total, int *error_code) {
    char note[8];
    note[0] = 0;
    memset(status, 0, 16);
    *downloaded = 0;
    *total = 0;
    *error_code = 0;
    pthread_mutex_lock(&lock);
    creds saved = creds_acquire(note, sizeof(note));
    init_once();
    memset(&status_buf, 0, sizeof(status_buf));
    int rc = patch ? sceAppInstUtilGetPatchInstallStatus(content_id, &status_buf)
                   : sceAppInstUtilGetInstallStatus(content_id, &status_buf);
    if (rc == 0) {
        memcpy(status, status_buf.status, 15);
        *downloaded = status_buf.downloaded;
        *total = status_buf.total;
        *error_code = status_buf.error.code;
    }
    creds_release(&saved, note, sizeof(note));
    pthread_mutex_unlock(&lock);
    return rc;
}

/* GetAddcontInstalledStatus: the native rc, `*state` 4 when the DLC is installed. 0x80E90004
 * (not found) until the install is promoted. */
int km_addcont_installed(const char *content_id, int *state) {
    char note[8];
    note[0] = 0;
    *state = 0;
    pthread_mutex_lock(&lock);
    creds saved = creds_acquire(note, sizeof(note));
    init_once();
    int rc = sceAppInstUtilGetAddcontInstalledStatus(content_id, state);
    creds_release(&saved, note, sizeof(note));
    pthread_mutex_unlock(&lock);
    return rc;
}

/* CancelInstall addresses the base application (SMP): the caller uses it only for a fresh base
 * install, never for a patch, an addcont or a base that already finished. */
int km_pkg_cancel(const char *content_id) {
    char note[8];
    note[0] = 0;
    pthread_mutex_lock(&lock);
    creds saved = creds_acquire(note, sizeof(note));
    int rc = sceAppInstUtilCancelInstall(content_id);
    creds_release(&saved, note, sizeof(note));
    pthread_mutex_unlock(&lock);
    return rc;
}
