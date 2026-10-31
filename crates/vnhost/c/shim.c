/* Variadic libc entry points for vnhost (see src/wrap.rs). */

#define _GNU_SOURCE
#include <stdarg.h>
#include <sys/ioctl.h>
#include <fcntl.h>

extern int vnhost_is_ours( int fd );
extern int vnhost_fcntl( int fd, int cmd, int arg );
extern int vnhost_fionread( int fd );
extern int vnhost_ntsync_ioctl( int fd, int nr, void *arg );
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
    if (vnhost_is_ours( fd ))
    {
        /* no locks on our files: report every range unlocked */
        if (cmd == F_GETLK) { ((struct flock *)arg)->l_type = F_UNLCK; return 0; }
        return vnhost_fcntl( fd, cmd, (int)arg );
    }
    return __real_fcntl( fd, cmd, arg );
}

/* open()/openat() can be routed to the browser file system (src/vfs.rs) */
extern int vnhost_openat( int dirfd, const char *path, int flags, mode_t mode );

int __wrap_open( const char *path, int flags, ... )
{
    mode_t mode = 0;

    if (flags & (O_CREAT | O_TMPFILE))
    {
        va_list ap;
        va_start( ap, flags );
        mode = va_arg( ap, mode_t );
        va_end( ap );
    }
    return vnhost_openat( AT_FDCWD, path, flags, mode );
}

int __wrap_openat( int dirfd, const char *path, int flags, ... )
{
    mode_t mode = 0;

    if (flags & (O_CREAT | O_TMPFILE))
    {
        va_list ap;
        va_start( ap, flags );
        mode = va_arg( ap, mode_t );
        va_end( ap );
    }
    return vnhost_openat( dirfd, path, flags, mode );
}

int __wrap_creat( const char *path, mode_t mode )
{
    return vnhost_openat( AT_FDCWD, path, O_CREAT | O_WRONLY | O_TRUNC, mode );
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
        if (((req >> 8) & 0xff) == 'N') return vnhost_ntsync_ioctl( fd, req & 0xff, arg );
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

/* ---------------------------------------------------------------------------------------- */
/* processes (see src/proc.rs) */

#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

extern int vnhost_getpid( void );
extern void vnhost_proc_enter( int pid );
extern void vnhost_proc_leave( void );
extern void vnvm_dl_enter( int pid );  /* hangover/vndl.js */
extern int vnhost_proc_create( int app, int root );
extern void vnhost_proc_setenv( int pid, const char *name, const char *val );
extern void vnhost_fd_give( int fd, int pid );
extern int __real_pthread_create( pthread_t *thread, const pthread_attr_t *attr,
                                  void *(*start)(void *), void *arg );
extern void *__real_dlopen( const char *name, int flags );
extern void *__real_dlsym( void *handle, const char *name );
extern char *__real_dlerror( void );

struct thread_start
{
    void *(*start)(void *);
    void *arg;
    int pid;
    double created;
};

static void thread_cleanup( void *arg )
{
    vnhost_proc_leave();
}

static double now_secs(void)
{
    struct timespec ts;
    clock_gettime( CLOCK_REALTIME, &ts );  /* the same clock on every worker */
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void *thread_trampoline( void *arg )
{
    struct thread_start ts = *(struct thread_start *)arg;
    void *ret;

    free( arg );
    vnhost_proc_enter( ts.pid );
    vnvm_dl_enter( ts.pid );
    if (getenv( "VNVM_TRACE_PROC" )) fprintf( stderr, "vnhost: %.3f process %d: thread start %p(%p), created %.3f\n", now_secs(), ts.pid, ts.start, ts.arg, ts.created );
    pthread_cleanup_push( thread_cleanup, NULL );
    ret = ts.start( ts.arg );
    pthread_cleanup_pop( 1 );
    return ret;
}

/* new threads belong to the creator's process */
int __wrap_pthread_create( pthread_t *thread, const pthread_attr_t *attr,
                           void *(*start)(void *), void *arg )
{
    struct thread_start *ts = malloc( sizeof(*ts) );
    int ret;

    ts->start = start;
    ts->arg = arg;
    ts->pid = vnhost_getpid();
    ts->created = now_secs();
    if ((ret = __real_pthread_create( thread, attr, thread_trampoline, ts ))) free( ts );
    return ret;
}

/* The unix side of Wine, loaded once per process; set by vnhost_set_unix_module(). */
static char unix_module[4096];

void vnhost_set_unix_module( const char *path )
{
    snprintf( unix_module, sizeof(unix_module), "%s", path );
}

/* Loads a fresh instance of the unix module for the calling thread's process and runs
 * __wine_main() in it. dlopen() caches by name, so every process opens it through its own
 * symlink. */
static void run_process( int argc, char **argv )
{
    char link[4200];
    void *handle;
    void (*wine_main)( int, char ** );

    snprintf( link, sizeof(link), "/tmp/vnvm-%u", (unsigned)getuid() );
    mkdir( link, 0700 );
    snprintf( link, sizeof(link), "/tmp/vnvm-%u/unix-%u-%d.so", (unsigned)getuid(),
              (unsigned)time( NULL ), vnhost_getpid() );
    unlink( link );
    if (symlink( unix_module, link ))
    {
        perror( link );
        exit( 1 );
    }
    if (getenv( "VNVM_TRACE_PROC" )) fprintf( stderr, "vnhost: %.3f process %d: loading\n", now_secs(), vnhost_getpid() );
    handle = __real_dlopen( link, RTLD_NOW | RTLD_LOCAL );
    if (getenv( "VNVM_TRACE_PROC" )) fprintf( stderr, "vnhost: %.3f process %d: loaded\n", now_secs(), vnhost_getpid() );
    unlink( link );
    if (!handle || !(wine_main = __real_dlsym( handle, "__wine_main" )))
    {
        fprintf( stderr, "vnhost: loading %s: %s\n", unix_module, __real_dlerror() );
        exit( 1 );
    }
    wine_main( argc, argv );
}

/* Runs the first Wine process on the calling thread; never returns. */
void vnhost_run_root( int app, int argc, char **argv )
{
    int pid = vnhost_proc_create( app, 1 );

    if (pid < 0)
    {
        fprintf( stderr, "vnhost: no address space for the first process\n" );
        exit( 1 );
    }
    vnhost_proc_enter( pid );
    run_process( argc, argv );
    exit( 1 );
}

struct spawn
{
    int pid;
    int argc;
    char **argv;
};

static void *spawn_thread( void *arg )
{
    struct spawn *sp = arg;

    run_process( sp->argc, sp->argv );
    return NULL;
}

/* Starts a Wine process in place of fork() + exec(wine).
 * argv: the loader's argv ("wine", program, args...), copied;
 * env: NULL-terminated "NAME=value" settings for the child; server_fd is dup'ed for it.
 * Returns the new pid or -1. */
int vnhost_spawn( char **argv, char **env, int server_fd, int app )
{
    int pid = vnhost_proc_create( app, 0 ), argc, i, fd;
    struct spawn *sp;
    pthread_attr_t attr;
    pthread_t thread;
    struct thread_start *ts;
    char buf[32];

    if (pid < 0) return -1;
    for (argc = 0; argv[argc]; argc++) ;
    sp = malloc( sizeof(*sp) );
    sp->pid = pid;
    sp->argc = argc;
    sp->argv = calloc( argc + 1, sizeof(char *) );
    for (i = 0; i < argc; i++) sp->argv[i] = strdup( argv[i] );

    for (i = 0; env && env[i]; i++)
    {
        char *name = strdup( env[i] ), *eq = strchr( name, '=' );
        if (eq) *eq++ = 0;
        vnhost_proc_setenv( pid, name, eq );
        free( name );
    }
    if ((fd = dup( server_fd )) == -1) return -1;
    vnhost_fd_give( fd, pid );
    snprintf( buf, sizeof(buf), "%d", fd );
    vnhost_proc_setenv( pid, "WINESERVERSOCKET", buf );
    vnhost_proc_setenv( pid, "WINELOADERNOEXEC", "1" );  /* what exec'ing the loader would set */

    /* the thread belongs to the child from its first instruction */
    pthread_attr_init( &attr );
    pthread_attr_setstacksize( &attr, 8 << 20 );
    ts = malloc( sizeof(*ts) );
    ts->start = spawn_thread;
    ts->arg = sp;
    ts->pid = pid;
    ts->created = now_secs();
    if (__real_pthread_create( &thread, &attr, thread_trampoline, ts ))
    {
        pthread_attr_destroy( &attr );
        return -1;
    }
    pthread_attr_destroy( &attr );
    pthread_detach( thread );
    return pid;
}

/* ---------------------------------------------------------------------------------------- */
/* stdio on our descriptors: libc's FILE does raw syscalls, so these go through cookies */

#include <stdio.h>
#include <sys/types.h>
#include <unistd.h>

extern ssize_t __wrap_read( int fd, void *buf, size_t len );
extern ssize_t __wrap_write( int fd, const void *buf, size_t len );
extern off_t __wrap_lseek( int fd, off_t off, int whence );
extern int __wrap_close( int fd );
extern FILE *__real_fopen( const char *path, const char *mode );
extern FILE *__real_fdopen( int fd, const char *mode );

static ssize_t cookie_read( void *c, char *buf, size_t len ) { return __wrap_read( (int)(long)c, buf, len ); }
static ssize_t cookie_write( void *c, const char *buf, size_t len ) { return __wrap_write( (int)(long)c, buf, len ); }
static int cookie_close( void *c ) { return __wrap_close( (int)(long)c ); }
static int cookie_seek( void *c, off_t *off, int whence )
{
    off_t r = __wrap_lseek( (int)(long)c, *off, whence );
    if (r < 0) return -1;
    *off = r;
    return 0;
}

static FILE *cookie_file( int fd, const char *mode )
{
    cookie_io_functions_t io = { cookie_read, cookie_write, cookie_seek, cookie_close };
    return fopencookie( (void *)(long)fd, mode, io );
}

FILE *__wrap_fopen( const char *path, const char *mode )
{
    int flags = strchr( mode, '+' ) ? O_RDWR : (*mode == 'r' ? O_RDONLY : O_WRONLY), fd;
    FILE *f;

    if (*mode == 'w') flags |= O_CREAT | O_TRUNC;
    if (*mode == 'a') flags |= O_CREAT | O_APPEND;
    if (strchr( mode, 'x' )) flags |= O_EXCL;
    if (strchr( mode, 'e' )) flags |= O_CLOEXEC;
    if ((fd = vnhost_openat( AT_FDCWD, path, flags, 0666 )) < 0) return NULL;
    f = vnhost_is_ours( fd ) ? cookie_file( fd, mode ) : __real_fdopen( fd, mode );
    if (!f) __wrap_close( fd );
    return f;
}

FILE *__wrap_fdopen( int fd, const char *mode )
{
    return vnhost_is_ours( fd ) ? cookie_file( fd, mode ) : __real_fdopen( fd, mode );
}

/* Emscripten's sched_yield() is a synchronous call to the browser's main thread, which then had
 * the round trips of every spin loop in Wine and the games to serve. Every thread has a worker
 * of its own: nothing to yield to. */
int __wrap_sched_yield( void )
{
    return 0;
}
