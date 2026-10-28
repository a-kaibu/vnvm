/* Variadic libc entry points for vnhost (see src/wrap.rs). */

#include <stdarg.h>
#include <sys/ioctl.h>
#include <fcntl.h>

extern int vnhost_is_ours( int fd );
extern int vnhost_fcntl( int fd, int cmd, int arg );
extern int vnhost_fionread( int fd );
extern void *vnhost_mremap( void );
extern int __real_fcntl( int fd, int cmd, ... );
extern int __real_ioctl( int fd, int req, ... );

int __wrap_fcntl( int fd, int cmd, ... )
{
    va_list ap;
    long arg;

    va_start( ap, cmd );
    arg = va_arg( ap, long );
    va_end( ap );
    if (vnhost_is_ours( fd )) return vnhost_fcntl( fd, cmd, (int)arg );
    return __real_fcntl( fd, cmd, arg );
}

int __wrap_ioctl( int fd, int req, ... )
{
    va_list ap;
    void *arg;

    va_start( ap, req );
    arg = va_arg( ap, void * );
    va_end( ap );
    if (vnhost_is_ours( fd ))
    {
        if (req == FIONREAD)
        {
            int n = vnhost_fionread( fd );
            if (n < 0) return -1;
            *(int *)arg = n;
        }
        return 0;
    }
    return __real_ioctl( fd, req, arg );
}

void *__wrap_mremap( void *old, unsigned long old_len, unsigned long new_len, int flags, ... )
{
    return vnhost_mremap();
}
