#include "CmuxNextSharedRegionC.h"

#include <errno.h>
#include <fcntl.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>

int cmux_shared_buffer_region_create(size_t count, int *read_only) {
  static atomic_uint serial = 0;
  if (count == 0 || read_only == NULL) {
    errno = EINVAL;
    return -1;
  }
  // PSHMNAMLEN is 31: "/cmux-sb-", a pid and a serial fit.
  for (int attempt = 0; attempt < 8; ++attempt) {
    char name[32];
    snprintf(name, sizeof name, "/cmux-sb-%d-%u", (int)getpid(), atomic_fetch_add(&serial, 1u));
    const int writable = shm_open(name, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (writable < 0) {
      if (errno == EEXIST) continue;
      return -1;
    }
    const int readable = shm_open(name, O_RDONLY, 0);
    const int saved = errno;
    shm_unlink(name);
    if (readable < 0) {
      close(writable);
      errno = saved;
      return -1;
    }
    // xnu sizes a POSIX shm object once: nobody can shrink it afterwards.
    // shm_open takes no O_CLOEXEC on every macOS, so it is set after, as in
    // shared_region.c.
    if (ftruncate(writable, (off_t)count) != 0 || fcntl(writable, F_SETFD, FD_CLOEXEC) != 0 ||
        fcntl(readable, F_SETFD, FD_CLOEXEC) != 0) {
      const int failed = errno;
      close(writable);
      close(readable);
      errno = failed;
      return -1;
    }
    *read_only = readable;
    return writable;
  }
  errno = EEXIST;
  return -1;
}
