/* Signals for a thread blocked in a syscall. A real kernel interrupts the syscall to run the
 * handler; vnhost's blocking calls that need it (the ntsync waits of in-process sync, which the
 * wineserver interrupts with SIGUSR1 to suspend a thread or hand it a system APC) call this and
 * then fail with EINTR. Linked into the unix side module. */
#include <signal.h>
#include "ntstatus.h"
#define WIN32_NO_STATUS
#include "windef.h"
#include "winternl.h"

extern void wait_suspend( CONTEXT *context );
extern void DECLSPEC_NORETURN abort_thread( int status );
extern void vnhost_set_signal_hook( void (*hook)( int sig ) );

static void syscall_signal( int sig )
{
    if (sig == SIGUSR1)
    {
        CONTEXT context = { CONTEXT_FULL | CONTEXT_FLOATING_POINT | CONTEXT_EXTENDED_REGISTERS };

        NtGetContextThread( NtCurrentThread(), &context );
        wait_suspend( &context );
        NtSetContextThread( NtCurrentThread(), &context );
    }
    else if (sig == SIGQUIT) abort_thread( 0 );
}

static void __attribute__((constructor)) init( void )
{
    vnhost_set_signal_hook( syscall_signal );
}
