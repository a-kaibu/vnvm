/* Locked instructions from several threads at once: every counter must come out exact, and a
 * critical section must keep its plain counter consistent.
 * Build: i686-w64-mingw32-gcc -O2 atomics.c -o atomics.exe */
#include <stdio.h>
#include <windows.h>

#define THREADS 4
#define N 200000

static volatile LONG inc, add, cas;
static volatile LONGLONG cas64;
static volatile short add16;
static CRITICAL_SECTION cs;
static int plain;

static DWORD WINAPI worker(void *arg)
{
    for (int i = 0; i < N; i++)
    {
        LONG v;
        LONGLONG w;

        InterlockedIncrement(&inc);
        InterlockedExchangeAdd(&add, 3);
        do v = cas; while (InterlockedCompareExchange(&cas, v + 1, v) != v);
        do w = cas64; while (InterlockedCompareExchange64(&cas64, w + 1, w) != w);
        __atomic_fetch_add(&add16, 1, __ATOMIC_SEQ_CST);
        if (!(i & 15))
        {
            EnterCriticalSection(&cs);
            plain++;
            LeaveCriticalSection(&cs);
        }
    }
    return 0;
}

int main(void)
{
    HANDLE h[THREADS];
    int ok;

    InitializeCriticalSection(&cs);
    for (int i = 0; i < THREADS; i++) h[i] = CreateThread(NULL, 0, worker, NULL, 0, NULL);
    WaitForMultipleObjects(THREADS, h, TRUE, INFINITE);
    ok = inc == THREADS * N && add == THREADS * N * 3 && cas == THREADS * N && cas64 == THREADS * N &&
         add16 == (short)(THREADS * N) && plain == THREADS * (N / 16);
    printf("inc %ld add %ld cas %ld cas64 %lld add16 %d plain %d: %s\n", inc, add, cas, cas64, add16, plain,
           ok ? "ok" : "WRONG");
    return !ok;
}
