/*
 * Entry point of the wasm build of Wine (vnvm Hangover-style runtime).
 *
 * This module holds wineserver and the host runtime (vnhost); Wine's unix side is a separate
 * side module that every Wine process loads its own instance of. The server runs on its own
 * thread, then the main thread becomes the first Wine process.
 *
 * usage: main [program.exe [args...]]
 * env: VNVM_UNIX_MODULE  the unix side module (set by pre.js)
 *      VNVM_ROOT_APP     1/0: whether the first process gets the big low address range
 */

#include <ctype.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

extern int wineserver_main( int argc, char *argv[] );
extern void vnhost_set_unix_module( const char *path );
extern void vnhost_run_root( int app, int argc, char **argv );

static void *server_thread( void *arg )
{
    char *argv[] = { "wineserver", "-f", NULL };
    if (getenv( "VNVM_SERVER_DEBUG" )) argv[1] = "-fd";
    wineserver_main( 2, argv );
    return NULL;
}

/* Programs of Wine itself run in a small slice of the address space; anything else may be a
 * game that wants its exe at 0x400000 and plenty of memory. */
static int is_app( const char *prog )
{
    const char *env = getenv( "VNVM_ROOT_APP" );
    char lower[1024];
    size_t i;

    if (env) return atoi( env );
    if (!prog || (!strchr( prog, '\\' ) && !strchr( prog, '/' ))) return 0;
    for (i = 0; prog[i] && i < sizeof(lower) - 1; i++) lower[i] = tolower( (unsigned char)prog[i] );
    lower[i] = 0;
    return !strstr( lower, "windows\\system32" ) && !strstr( lower, "windows/system32" );
}

int main( int argc, char *argv[] )
{
    pthread_t server;
    const char *module = getenv( "VNVM_UNIX_MODULE" );
    char **wine_argv = calloc( argc + 1, sizeof(*wine_argv) );

    if (!module)
    {
        fprintf( stderr, "VNVM_UNIX_MODULE is not set\n" );
        return 1;
    }
    vnhost_set_unix_module( module );

    pthread_create( &server, NULL, server_thread, NULL );
    pthread_detach( server );

    /* we are the loader: don't let ntdll try to re-exec one */
    setenv( "WINELOADERNOEXEC", "1", 1 );
    wine_argv[0] = "wine";
    for (int i = 1; i < argc; i++) wine_argv[i] = argv[i];
    vnhost_run_root( is_app( argv[1] ), argc, wine_argv );
    return 1;
}
