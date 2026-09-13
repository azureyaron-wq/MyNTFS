/*
 * Open /dev/rdiskNsM the way Apple documents for GUI disk tools:
 * AuthorizationCreate + /usr/libexec/authopen -stdoutpipe -extauth
 * (Raspberry Pi Imager macfile.cpp; Apple Secure Coding Guide).
 *
 * Do not use osascript "do shell script … with administrator privileges".
 * That root process has no TCC identity, so open(O_RDWR) returns EPERM
 * even after a successful unmount.
 */

#include "myntfs.h"

#include <Security/Authorization.h>
#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static int valid_rdisk(const char *p) {
    if (strncmp(p, "/dev/rdisk", 10) != 0) {
        return 0;
    }
    p += 10;
    if (!isdigit((unsigned char)*p)) {
        return 0;
    }
    while (isdigit((unsigned char)*p)) {
        p++;
    }
    if (*p != 's') {
        return 0;
    }
    p++;
    if (!isdigit((unsigned char)*p)) {
        return 0;
    }
    while (isdigit((unsigned char)*p)) {
        p++;
    }
    return *p == '\0';
}

static void seterr(char *err, size_t n, const char *msg) {
    if (err && n) {
        strlcpy(err, msg ? msg : "authopen failed", n);
    }
}

int myntfs_authopen_rdisk(const char *rdisk, char *errbuf, size_t errbuf_len) {
    if (!rdisk || !valid_rdisk(rdisk)) {
        seterr(errbuf, errbuf_len, "invalid rdisk path");
        return -1;
    }

    char right[160];
    snprintf(right, sizeof right, "sys.openfile.readwrite.%s", rdisk);

    AuthorizationItem item = {right, 0, NULL, 0};
    AuthorizationRights rights = {1, &item};
    const char *prompt =
        "MyNTFS needs to open this USB disk for writing. The volume will stay "
        "unmounted in Finder while MyNTFS has exclusive access.";
    AuthorizationItem envItems[] = {
        {"prompt", strlen(prompt), (void *)prompt, 0}
    };
    AuthorizationEnvironment env = {1, envItems};
    AuthorizationFlags flags = kAuthorizationFlagDefaults |
                               kAuthorizationFlagInteractionAllowed |
                               kAuthorizationFlagExtendRights |
                               kAuthorizationFlagPreAuthorize;

    AuthorizationRef authRef = NULL;
    OSStatus st = AuthorizationCreate(&rights, &env, flags, &authRef);
    if (st == errAuthorizationCanceled || st == errAuthorizationDenied) {
        seterr(errbuf, errbuf_len, "authorization cancelled");
        return -1;
    }
    if (st != errAuthorizationSuccess || !authRef) {
        seterr(errbuf, errbuf_len, "could not create authorization");
        return -1;
    }

    AuthorizationExternalForm externalForm;
    if (AuthorizationMakeExternalForm(authRef, &externalForm) != errAuthorizationSuccess) {
        AuthorizationFree(authRef, kAuthorizationFlagDefaults);
        seterr(errbuf, errbuf_len, "could not serialize authorization");
        return -1;
    }

    int sock[2] = {-1, -1};
    int inpipe[2] = {-1, -1};
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sock) != 0 || pipe(inpipe) != 0) {
        AuthorizationFree(authRef, kAuthorizationFlagDefaults);
        seterr(errbuf, errbuf_len, "could not create authopen pipes");
        return -1;
    }

    char mode[16];
    snprintf(mode, sizeof mode, "%d", O_RDWR);

    pid_t pid = fork();
    if (pid < 0) {
        close(sock[0]);
        close(sock[1]);
        close(inpipe[0]);
        close(inpipe[1]);
        AuthorizationFree(authRef, kAuthorizationFlagDefaults);
        seterr(errbuf, errbuf_len, "fork failed");
        return -1;
    }
    if (pid == 0) {
        close(sock[0]);
        close(inpipe[1]);
        dup2(sock[1], STDOUT_FILENO);
        dup2(inpipe[0], STDIN_FILENO);
        close(sock[1]);
        close(inpipe[0]);
        execl("/usr/libexec/authopen", "authopen", "-stdoutpipe", "-extauth", "-o", mode, rdisk, (char *)NULL);
        _exit(127);
    }

    close(sock[1]);
    close(inpipe[0]);
    ssize_t wr = write(inpipe[1], externalForm.bytes, sizeof externalForm.bytes);
    close(inpipe[1]);
    AuthorizationFree(authRef, kAuthorizationFlagDefaults);
    if (wr != (ssize_t)sizeof externalForm.bytes) {
        close(sock[0]);
        waitpid(pid, NULL, 0);
        seterr(errbuf, errbuf_len, "failed to send authorization to authopen");
        return -1;
    }

    char iovbuf[CMSG_SPACE(sizeof(int))];
    char cmsgbuf[CMSG_SPACE(sizeof(int))];
    struct iovec iov = {.iov_base = iovbuf, .iov_len = sizeof iovbuf};
    struct msghdr msg;
    memset(&msg, 0, sizeof msg);
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsgbuf;
    msg.msg_controllen = sizeof cmsgbuf;

    ssize_t n;
    do {
        n = recvmsg(sock[0], &msg, 0);
    } while (n < 0 && errno == EINTR);
    close(sock[0]);

    int fd = -1;
    if (n > 0) {
        struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
        if (cmsg && cmsg->cmsg_level == SOL_SOCKET && cmsg->cmsg_type == SCM_RIGHTS &&
            cmsg->cmsg_len >= CMSG_LEN(sizeof(int))) {
            memcpy(&fd, CMSG_DATA(cmsg), sizeof(int));
        }
    }

    int status = 0;
    waitpid(pid, &status, 0);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        if (fd >= 0) {
            close(fd);
        }
        int code = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
        if (code == 1 && n <= 0) {
            seterr(errbuf, errbuf_len,
                   "authopen could not open the disk. Unmount it first, or grant MyNTFS access to Removable Volumes / Full Disk Access.");
        } else {
            char buf[160];
            snprintf(buf, sizeof buf, "authopen failed (exit %d)", code);
            seterr(errbuf, errbuf_len, buf);
        }
        return -1;
    }
    if (fd < 0) {
        seterr(errbuf, errbuf_len, "authopen did not return a file descriptor");
        return -1;
    }
    int fl = fcntl(fd, F_GETFL);
    if (fl < 0 || (fl & O_ACCMODE) != O_RDWR) {
        close(fd);
        seterr(errbuf, errbuf_len, "authopen fd is not O_RDWR");
        return -1;
    }
    return fd;
}

int myntfs_fd_getfl(int fd) {
    if (fd < 0) {
        return -1;
    }
    return fcntl(fd, F_GETFL);
}

int myntfs_fd_writable(int fd) {
    int fl = myntfs_fd_getfl(fd);
    if (fl < 0) {
        return 0;
    }
    int acc = fl & O_ACCMODE;
    return acc == O_RDWR || acc == O_WRONLY;
}

int myntfs_fd_pread_ok(int fd) {
    if (fd < 0) {
        return 0;
    }
    char buf[512];
    return pread(fd, buf, sizeof buf, 0) == (ssize_t)sizeof buf ? 1 : 0;
}
