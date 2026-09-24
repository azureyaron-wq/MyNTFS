#ifndef MYNTFS_H
#define MYNTFS_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct MyNtfsVolume MyNtfsVolume;

typedef struct MyNtfsSafetyReport {
    int verified;
    int dirty;
    int hibernated;
    int bitlocker;
    int efs_present;
    int writable_safe;
} MyNtfsSafetyReport;

/* Probe safety gates without mounting. Returns 0 on success. */
int myntfs_probe(const char *path, MyNtfsSafetyReport *out, char *errbuf, size_t errbuf_len);

/* Mount read-only by default. Block-device write requires myntfs_mount_ex. */
MyNtfsVolume *myntfs_mount(const char *path, int writable, char *errbuf, size_t errbuf_len);

MyNtfsVolume *myntfs_mount_ex(const char *path, int writable, int allow_device_write,
                                char *errbuf, size_t errbuf_len);

/* Mount from an already-open FD. Duplicates fd; caller may close theirs. Never
 * reopens display_path. Use this for USB after authopen. */
MyNtfsVolume *myntfs_mount_fd(int fd, const char *display_path, int writable,
                              int allow_device_write, char *errbuf, size_t errbuf_len);

void myntfs_umount(MyNtfsVolume *vol);

/* Flush, reset $LogFile, clear dirty — so Finder and Windows see the writes. 0 = ok. */
int myntfs_sync(MyNtfsVolume *vol);

int myntfs_is_writable(const MyNtfsVolume *vol);

int myntfs_volume_serial(const MyNtfsVolume *vol, uint64_t *out_serial);

/* total and free bytes. 0 = ok. */
int myntfs_volume_space(MyNtfsVolume *vol, uint64_t *out_total, uint64_t *out_free);

int myntfs_volume_safety(const MyNtfsVolume *vol, MyNtfsSafetyReport *out);

int myntfs_listdir(MyNtfsVolume *vol, const char *path, char *buf, size_t buf_len,
                   uint8_t *is_dir, uint64_t *sizes, int max_entries);

int64_t myntfs_read(MyNtfsVolume *vol, const char *path, uint64_t offset,
                    void *buf, size_t length);

int64_t myntfs_stat_size(MyNtfsVolume *vol, const char *path, int *is_dir);

int myntfs_mkdir(MyNtfsVolume *vol, const char *parent, const char *name);
int myntfs_create(MyNtfsVolume *vol, const char *parent, const char *name);
int64_t myntfs_write_contents(MyNtfsVolume *vol, const char *path,
                              const void *data, size_t len);
int myntfs_unlink(MyNtfsVolume *vol, const char *path);
int myntfs_rmdir(MyNtfsVolume *vol, const char *path);
/* File, or folder plus everything inside it. */
int myntfs_remove(MyNtfsVolume *vol, const char *path);
int myntfs_rename(MyNtfsVolume *vol, const char *old_path, const char *new_basename);

/* Unmount Finder/Paragon so /dev/rdisk* can be opened. */
int myntfs_claim(const char *bsd, char *errbuf, size_t errbuf_len);

int64_t myntfs_copy_out(MyNtfsVolume *vol, const char *ntfs_path,
                        const char *dest_host_path);

int64_t myntfs_copy_in(MyNtfsVolume *vol, const char *src_host_path,
                       const char *parent, const char *name);

int myntfs_format(const char *image_path, uint64_t size_bytes, const char *label,
                  char *errbuf, size_t errbuf_len);

/* List external USB volumes (NTFS and others). Each line is:
 *   bsd|volume_name|mount_point|rdisk|raw_ok|fs_kind|is_ntfs
 * empty fields are allowed. is_ntfs is 1 or 0. Returns line count, or -1. */
int myntfs_list_disks(char *buf, size_t buf_len);

const char *myntfs_last_error(void);

/* DiskArbitration exclusive hold: mount veto + force-unmount + claim. */
int myntfs_da_hold(const char *bsd, char *errbuf, size_t errbuf_len);
void myntfs_da_release(void);
int myntfs_da_holding(void);

/* Apple authopen(1) RDWR open of an allowlisted /dev/rdiskNsM. Returns fd or -1. */
int myntfs_authopen_rdisk(const char *rdisk, char *errbuf, size_t errbuf_len);

int myntfs_fd_writable(int fd);
int myntfs_fd_pread_ok(int fd);
int myntfs_fd_getfl(int fd);

int myntfs_da_slice_mounted(const char *bsd);
int myntfs_da_ensure_unmounted(char *errbuf, size_t errbuf_len);

/* After da_release: DADiskMount + poll until /Volumes/… (≤10s). Returns 0 and
 * copies the mount path. Callers must not hold exclusive access. */
int myntfs_da_mount_finder(const char *bsd, char *pathbuf, size_t pathbuf_len,
                           char *errbuf, size_t errbuf_len);

#ifdef __cplusplus
}
#endif

#endif
