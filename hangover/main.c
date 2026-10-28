/*
 * Entry point of the wasm build of Wine (vnvm Hangover-style runtime).
 *
 * One wasm module holds wineserver and one Wine process: the server runs on its own thread,
 * then the main thread becomes the Wine process (__wine_main never returns).
 *
 * usage: main [program.exe [args...]]
 */

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

extern int wineserver_main( int argc, char *argv[] );
extern void __wine_main( int argc, char *argv[] );

static void *server_thread( void *arg )
{
    char *argv[] = { "wineserver", "-f", NULL };
    if (getenv( "VNVM_SERVER_DEBUG" )) argv[1] = "-fd";
    wineserver_main( 2, argv );
    return NULL;
}

int main( int argc, char *argv[] )
{
    pthread_t server;
    char **wine_argv = calloc( argc + 1, sizeof(*wine_argv) );

    pthread_create( &server, NULL, server_thread, NULL );
    pthread_detach( server );

    /* we are the loader: don't let ntdll try to re-exec one */
    setenv( "WINELOADERNOEXEC", "1", 1 );
    wine_argv[0] = "wine";
    for (int i = 1; i < argc; i++) wine_argv[i] = argv[i];
    __wine_main( argc, wine_argv );
    return 1;
}
