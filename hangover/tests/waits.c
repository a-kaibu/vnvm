/* Timed waits, event ping-pong between threads, mutex/semaphore/wait-all semantics (in-process
 * sync), with timings. Build: i686-w64-mingw32-gcc -O2 waits.c -o waits.exe */
#include <stdio.h>
#include <windows.h>

static HANDLE ping, pong;

static DWORD WINAPI ponger(void *arg)
{
    for (int i = 0; i < 10000; i++)
    {
        WaitForSingleObject(ping, INFINITE);
        SetEvent(pong);
    }
    return 0;
}

static double now(void)
{
    LARGE_INTEGER c, f;
    QueryPerformanceCounter(&c);
    QueryPerformanceFrequency(&f);
    return (double)c.QuadPart * 1000 / f.QuadPart;
}

int main(void)
{
    HANDLE e = CreateEventA(NULL, FALSE, FALSE, NULL), t;
    setvbuf(stdout, NULL, _IONBF, 0);
    double t0 = now();
    for (int i = 0; i < 50; i++) WaitForSingleObject(e, 10);
    printf("50 x wait(10ms): %.0f ms\n", now() - t0);
    t0 = now();
    for (int i = 0; i < 50; i++) Sleep(10);
    printf("50 x Sleep(10): %.0f ms\n", now() - t0);
    ping = CreateEventA(NULL, FALSE, FALSE, NULL);
    pong = CreateEventA(NULL, FALSE, FALSE, NULL);
    t = CreateThread(NULL, 0, ponger, NULL, 0, NULL);
    t0 = now();
    for (int i = 0; i < 10000; i++)
    {
        SetEvent(ping);
        WaitForSingleObject(pong, INFINITE);
    }
    printf("10000 ping-pongs: %.0f ms\n", now() - t0);
    WaitForSingleObject(t, INFINITE);

    HANDLE m = CreateMutexA(NULL, FALSE, NULL), s = CreateSemaphoreA(NULL, 0, 2, NULL);
    LONG prev = -1;
    DWORD r[8];
    /* expected: mutex 0 0 1 1 0 */
    r[0] = WaitForSingleObject(m, 0);
    r[1] = WaitForSingleObject(m, 0);
    r[2] = ReleaseMutex(m);
    r[3] = ReleaseMutex(m);
    r[4] = ReleaseMutex(m);
    printf("mutex %lu %lu %lu %lu %lu\n", r[0], r[1], r[2], r[3], r[4]);
    /* expected: sem 1 0 0 0 258 0 */
    r[0] = ReleaseSemaphore(s, 2, &prev);
    r[1] = WaitForSingleObject(s, 0);
    r[2] = WaitForSingleObject(s, 0);
    r[3] = WaitForSingleObject(s, 0);
    r[4] = ReleaseSemaphore(s, 3, NULL);
    printf("sem %lu %ld %lu %lu %lu %lu\n", r[0], prev, r[1], r[2], r[3], r[4]);
    /* expected: all 258 any 1 all 0 */
    HANDLE two[2] = { e, ping };
    SetEvent(e);
    r[0] = WaitForMultipleObjects(2, two, TRUE, 0);
    SetEvent(ping);
    r[1] = WaitForMultipleObjects(2, two, FALSE, 0);
    SetEvent(ping);
    r[2] = WaitForMultipleObjects(2, two, TRUE, 0);
    printf("all %lu any %lu all %lu\n", r[0], r[1], r[2]);
    return 0;
}
