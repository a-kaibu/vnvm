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
};

static void thread_cleanup( void *arg )
{
    vnhost_proc_leave();
}

static void *thread_trampoline( void *arg )
{
    struct thread_start ts = *(struct thread_start *)arg;
    void *ret;

    free( arg );
    vnhost_proc_enter( ts.pid );
    if (getenv( "VNVM_TRACE_PROC" )) fprintf( stderr, "vnhost: process %d: thread start %p(%p)\n", ts.pid, ts.start, ts.arg );
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
    if (getenv( "VNVM_TRACE_PROC" )) fprintf( stderr, "vnhost: process %d: loading\n", vnhost_getpid() );
    handle = __real_dlopen( link, RTLD_NOW | RTLD_LOCAL );
    if (getenv( "VNVM_TRACE_PROC" )) fprintf( stderr, "vnhost: process %d: loaded\n", vnhost_getpid() );
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
    if (__real_pthread_create( &thread, &attr, thread_trampoline, ts ))
    {
        pthread_attr_destroy( &attr );
        return -1;
    }
    pthread_attr_destroy( &attr );
    pthread_detach( thread );
    return pid;
}
