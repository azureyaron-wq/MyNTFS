/*
 * Exclusive DiskArbitration hold for one external NTFS slice.
 *
 * Apple’s documented pattern (Disk Arbitration Programming Guide):
 *   1. Register DARegisterDiskMountApprovalCallback and dissent remounts
 *   2. DADiskUnmount(kDADiskUnmountOptionForce) on that slice
 *   3. DADiskClaim so diskarbitrationd / FSKit cannot take the volume back
 *
 * diskutil unmount is fire-and-forget and does not take a claim; FSKit then
 * remounts during the password sheet. This session stays alive until
 * myntfs_da_release().
 */

#include "myntfs.h"

#include <CoreFoundation/CoreFoundation.h>
#include <DiskArbitration/DiskArbitration.h>
#include <dispatch/dispatch.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/param.h>
#include <sys/wait.h>
#include <unistd.h>

static pthread_mutex_t g_mu = PTHREAD_MUTEX_INITIALIZER;
static DASessionRef g_session;
static DAApprovalSessionRef g_approval;
static dispatch_queue_t g_queue;
static DADiskRef g_disk;
static char g_bsd[32];
static int g_claimed;
static int g_veto_registered;

typedef struct {
    dispatch_semaphore_t sem;
    DAReturn status;
    char reason[256];
} DaWait;

static void seterr(char *err, size_t n, const char *msg) {
    if (err && n) {
        strlcpy(err, msg ? msg : "unknown DiskArbitration error", n);
    }
}

static int valid_slice(const char *name) {
    if (strncmp(name, "disk", 4) != 0) {
        return 0;
    }
    const char *p = name + 4;
    if (*p < '0' || *p > '9') {
        return 0;
    }
    while (*p >= '0' && *p <= '9') {
        p++;
    }
    if (*p != 's') {
        return 0;
    }
    p++;
    if (*p < '0' || *p > '9') {
        return 0;
    }
    while (*p >= '0' && *p <= '9') {
        p++;
    }
    return *p == '\0';
}

static int slice_is_mounted(const char *bsd) {
    struct statfs *mnts = NULL;
    int n = getmntinfo(&mnts, MNT_NOWAIT);
    if (n <= 0 || !mnts) {
        return 0;
    }
    char diskdev[64];
    char rdiskdev[64];
    snprintf(diskdev, sizeof diskdev, "/dev/%s", bsd);
    snprintf(rdiskdev, sizeof rdiskdev, "/dev/r%s", bsd);
    for (int i = 0; i < n; i++) {
        if (strcmp(mnts[i].f_mntfromname, diskdev) == 0 ||
            strcmp(mnts[i].f_mntfromname, rdiskdev) == 0) {
            return 1;
        }
    }
    return 0;
}

static int is_system_volume(CFDictionaryRef desc, const char *bsd) {
    if (strncmp(bsd, "disk0", 5) == 0 && (bsd[5] == '\0' || bsd[5] == 's')) {
        return 1;
    }
    CFBooleanRef intern = CFDictionaryGetValue(desc, kDADiskDescriptionDeviceInternalKey);
    if (intern == kCFBooleanTrue) {
        return 1;
    }
    CFURLRef path = CFDictionaryGetValue(desc, kDADiskDescriptionVolumePathKey);
    if (path && CFGetTypeID(path) == CFURLGetTypeID()) {
        char buf[MAXPATHLEN];
        if (CFURLGetFileSystemRepresentation(path, true, (UInt8 *)buf, sizeof buf)) {
            if (strcmp(buf, "/") == 0 || strncmp(buf, "/System/Volumes/", 16) == 0) {
                return 1;
            }
        }
    }
    return 0;
}

static int is_external_ok(CFDictionaryRef desc) {
    CFBooleanRef intern = CFDictionaryGetValue(desc, kDADiskDescriptionDeviceInternalKey);
    if (intern == kCFBooleanTrue) {
        return 0;
    }
    CFStringRef proto = CFDictionaryGetValue(desc, kDADiskDescriptionDeviceProtocolKey);
    if (proto && CFGetTypeID(proto) == CFStringGetTypeID()) {
        if (CFStringCompare(proto, CFSTR("USB"), kCFCompareCaseInsensitive) == kCFCompareEqualTo) {
            return 1;
        }
    }
    CFBooleanRef rem = CFDictionaryGetValue(desc, kDADiskDescriptionMediaRemovableKey);
    if (rem == kCFBooleanTrue) {
        return 1;
    }
    return intern == kCFBooleanFalse;
}

static void wait_done(DADiskRef disk, DADissenterRef dissenter, void *context) {
    (void)disk;
    DaWait *w = context;
    if (dissenter) {
        w->status = DADissenterGetStatus(dissenter);
        CFStringRef s = DADissenterGetStatusString(dissenter);
        if (s) {
            CFStringGetCString(s, w->reason, sizeof w->reason, kCFStringEncodingUTF8);
        }
    } else {
        w->status = kDAReturnSuccess;
        w->reason[0] = 0;
    }
    dispatch_semaphore_signal(w->sem);
}

static DAReturn wait_op(dispatch_semaphore_t sem, char *reason, size_t n, int timeout_sec) {
    if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, (int64_t)timeout_sec * NSEC_PER_SEC)) != 0) {
        if (reason && n) {
            strlcpy(reason, "DiskArbitration timed out", n);
        }
        return kDAReturnBusy;
    }
    return kDAReturnSuccess;
}

static DADissenterRef mount_veto(DADiskRef disk, void *context) {
    (void)context;
    const char *name = DADiskGetBSDName(disk);
    if (name && g_bsd[0] && strcmp(name, g_bsd) == 0) {
        fprintf(stderr, "MYNTFS_DA veto remount of %s\n", name);
        return DADissenterCreate(kCFAllocatorDefault, kDAReturnExclusiveAccess,
                                 CFSTR("MyNTFS has exclusive access to this volume"));
    }
    return NULL;
}

static DADissenterRef claim_release(DADiskRef disk, void *context) {
    (void)disk;
    (void)context;
    return DADissenterCreate(kCFAllocatorDefault, kDAReturnBusy,
                             CFSTR("MyNTFS is using this volume"));
}

static void da_teardown_locked(void) {
    if (g_disk && g_claimed) {
        DADiskUnclaim(g_disk);
        g_claimed = 0;
    }
    if (g_approval && g_veto_registered) {
        DAUnregisterApprovalCallback(g_approval, mount_veto, NULL);
        g_veto_registered = 0;
    }
    if (g_approval) {
        DASessionSetDispatchQueue((DASessionRef)g_approval, NULL);
        CFRelease(g_approval);
        g_approval = NULL;
    }
    if (g_session) {
        DASessionSetDispatchQueue(g_session, NULL);
        CFRelease(g_session);
        g_session = NULL;
    }
    if (g_disk) {
        CFRelease(g_disk);
        g_disk = NULL;
    }
    g_bsd[0] = 0;
    g_queue = NULL;
}

void myntfs_da_release(void) {
    pthread_mutex_lock(&g_mu);
    da_teardown_locked();
    pthread_mutex_unlock(&g_mu);
}

int myntfs_da_holding(void) {
    pthread_mutex_lock(&g_mu);
    int held = g_session != NULL && g_bsd[0] != 0;
    pthread_mutex_unlock(&g_mu);
    return held;
}

static const char *bsd_name(const char *p) {
    if (!p || !p[0]) {
        return "";
    }
    if (strncmp(p, "/dev/rdisk", 10) == 0) {
        return p + 6;
    }
    if (strncmp(p, "/dev/", 5) == 0) {
        return p + 5;
    }
    return p;
}

int myntfs_da_slice_mounted(const char *bsd) {
    return slice_is_mounted(bsd_name(bsd));
}

static int exec_unmount_force(const char *bsd);

int myntfs_da_ensure_unmounted(char *errbuf, size_t errbuf_len) {
    pthread_mutex_lock(&g_mu);
    if (g_bsd[0] == 0) {
        pthread_mutex_unlock(&g_mu);
        seterr(errbuf, errbuf_len, "no DiskArbitration hold");
        return -1;
    }
    char bsd[32];
    strlcpy(bsd, g_bsd, sizeof bsd);
    pthread_mutex_unlock(&g_mu);
    for (int i = 0; i < 8 && slice_is_mounted(bsd); i++) {
        exec_unmount_force(bsd);
        usleep(150000);
    }
    if (slice_is_mounted(bsd)) {
        seterr(errbuf, errbuf_len, "still mounted after DiskArbitration unmount force (FSKit)");
        return -1;
    }
    return 0;
}

static int exec_unmount_force(const char *bsd) {
    pid_t pid = fork();
    if (pid < 0) {
        return -1;
    }
    if (pid == 0) {
        char *argv[] = {"/usr/sbin/diskutil", "unmount", "force", (char *)bsd, NULL};
        execv(argv[0], argv);
        _exit(127);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    return WIFEXITED(status) ? WEXITSTATUS(status) : -1;
}

int myntfs_da_hold(const char *bsd, char *errbuf, size_t errbuf_len) {
    if (!bsd || !valid_slice(bsd)) {
        seterr(errbuf, errbuf_len, "invalid disk slice");
        return -1;
    }

    pthread_mutex_lock(&g_mu);
    da_teardown_locked();

    strlcpy(g_bsd, bsd, sizeof g_bsd);
    g_queue = dispatch_queue_create("com.myntfs.diskarbitration", DISPATCH_QUEUE_SERIAL);
    g_session = DASessionCreate(kCFAllocatorDefault);
    g_approval = DAApprovalSessionCreate(kCFAllocatorDefault);
    if (!g_queue || !g_session || !g_approval) {
        da_teardown_locked();
        pthread_mutex_unlock(&g_mu);
        seterr(errbuf, errbuf_len, "could not create DiskArbitration session");
        return -1;
    }
    DASessionSetDispatchQueue(g_session, g_queue);
    DASessionSetDispatchQueue((DASessionRef)g_approval, g_queue);

    g_disk = DADiskCreateFromBSDName(kCFAllocatorDefault, g_session, bsd);
    if (!g_disk) {
        da_teardown_locked();
        pthread_mutex_unlock(&g_mu);
        seterr(errbuf, errbuf_len, "unknown BSD disk name");
        return -1;
    }
    CFDictionaryRef desc = DADiskCopyDescription(g_disk);
    if (!desc) {
        da_teardown_locked();
        pthread_mutex_unlock(&g_mu);
        seterr(errbuf, errbuf_len, "could not read disk description");
        return -1;
    }
    int sysvol = is_system_volume(desc, bsd);
    int ext = is_external_ok(desc);
    CFRelease(desc);
    if (sysvol) {
        da_teardown_locked();
        pthread_mutex_unlock(&g_mu);
        seterr(errbuf, errbuf_len, "refusing internal or boot disk");
        return -1;
    }
    if (!ext) {
        da_teardown_locked();
        pthread_mutex_unlock(&g_mu);
        seterr(errbuf, errbuf_len, "refusing non-external disk");
        return -1;
    }

    DARegisterDiskMountApprovalCallback(g_approval, NULL, mount_veto, NULL);
    g_veto_registered = 1;
    /* Give diskarbitrationd a beat to attach the approval session. */
    usleep(150000);

    DaWait un = {0};
    un.sem = dispatch_semaphore_create(0);
    un.status = kDAReturnSuccess;
    DADiskUnmount(g_disk, kDADiskUnmountOptionForce, wait_done, &un);
    pthread_mutex_unlock(&g_mu);

    char timed[256] = {0};
    if (wait_op(un.sem, timed, sizeof timed, 20) != kDAReturnSuccess) {
        pthread_mutex_lock(&g_mu);
        da_teardown_locked();
        pthread_mutex_unlock(&g_mu);
        seterr(errbuf, errbuf_len, timed[0] ? timed : "unmount timed out");
        return -1;
    }

    pthread_mutex_lock(&g_mu);
    if (un.status != kDAReturnSuccess && slice_is_mounted(bsd)) {
        /* DA unmount can fail if already unmounted; only treat as error if still mounted. */
        exec_unmount_force(bsd);
        usleep(200000);
    }
    for (int i = 0; i < 10 && slice_is_mounted(bsd); i++) {
        exec_unmount_force(bsd);
        usleep(150000);
    }
    if (slice_is_mounted(bsd)) {
        da_teardown_locked();
        pthread_mutex_unlock(&g_mu);
        seterr(errbuf, errbuf_len, "still mounted after DiskArbitration unmount force (FSKit)");
        return -1;
    }

    DaWait cl = {0};
    cl.sem = dispatch_semaphore_create(0);
    DADiskClaim(g_disk, kDADiskClaimOptionDefault, claim_release, NULL, wait_done, &cl);
    pthread_mutex_unlock(&g_mu);

    if (wait_op(cl.sem, timed, sizeof timed, 10) != kDAReturnSuccess) {
        pthread_mutex_lock(&g_mu);
        da_teardown_locked();
        pthread_mutex_unlock(&g_mu);
        seterr(errbuf, errbuf_len, "claim timed out");
        return -1;
    }
    pthread_mutex_lock(&g_mu);
    if (cl.status == kDAReturnSuccess) {
        g_claimed = 1;
    } else {
        /* Veto still active; claim is best-effort. */
        fprintf(stderr, "MYNTFS_DA claim status=0x%x %s\n", (unsigned)cl.status, cl.reason);
    }
    if (slice_is_mounted(bsd)) {
        da_teardown_locked();
        pthread_mutex_unlock(&g_mu);
        seterr(errbuf, errbuf_len, "volume remounted after claim");
        return -1;
    }
    pthread_mutex_unlock(&g_mu);
    fprintf(stderr, "MYNTFS_DA hold ok bsd=%s claimed=%d\n", bsd, g_claimed);
    return 0;
}

#ifdef MYNTFS_DAHOLD_PROBE
#include <stdlib.h>

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: dahold_probe <diskNsM> [seconds]\n");
        return 2;
    }
    const char *bsd = argv[1];
    int seconds = argc > 2 ? atoi(argv[2]) : 8;
    char err[512] = {0};
    if (myntfs_da_hold(bsd, err, sizeof err) != 0) {
        fprintf(stderr, "HOLD_FAIL %s\n", err);
        return 1;
    }
    printf("HOLD_OK mounted=%d\n", slice_is_mounted(bsd));
    char rpath[64];
    snprintf(rpath, sizeof rpath, "/dev/r%s", bsd);
    int fd = open(rpath, O_RDWR);
    if (fd >= 0) {
        printf("OPEN_RDWR fd=%d\n", fd);
        close(fd);
    } else {
        printf("OPEN_RDWR errno=%d %s\n", errno, strerror(errno));
    }
    fflush(stdout);
    if (seconds > 0) {
        sleep(seconds);
    }
    myntfs_da_release();
    printf("RELEASED mounted=%d\n", slice_is_mounted(bsd));
    return 0;
}
#endif
