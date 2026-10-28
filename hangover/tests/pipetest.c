/* named pipe smoke test: byte and message mode, blocking and overlapped reads */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static const char *name = "\\\\.\\pipe\\vnvmtest";
static DWORD mode;

static DWORD WINAPI client(void *arg)
{
    HANDLE h = CreateFileA(name, GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING, 0, NULL);
    char buf[300];
    DWORD n, i;
    if (h == INVALID_HANDLE_VALUE) { printf("client open failed %lu\n", GetLastError()); return 1; }
    for (i = 0; i < 5; i++)
    {
        memset(buf, 'a' + i, sizeof(buf));
        if (!WriteFile(h, buf, 100 + 50 * i, &n, NULL)) printf("write failed %lu\n", GetLastError());
    }
    /* read the reply */
    if (!ReadFile(h, buf, sizeof(buf), &n, NULL)) printf("client read failed %lu\n", GetLastError());
    else printf("client got %lu bytes '%.5s'\n", n, buf);
    CloseHandle(h);
    return 0;
}

static int run(DWORD type, BOOL overlapped)
{
    HANDLE h = CreateNamedPipeA(name, PIPE_ACCESS_DUPLEX | (overlapped ? FILE_FLAG_OVERLAPPED : 0),
                                type | (type == PIPE_TYPE_MESSAGE ? PIPE_READMODE_MESSAGE : 0) | PIPE_WAIT,
                                1, 4096, 4096, 0, NULL);
    OVERLAPPED ov = {0};
    char buf[400];
    DWORD n, i, total = 0, bad = 0;
    HANDLE t;
    ov.hEvent = CreateEventA(NULL, TRUE, FALSE, NULL);
    t = CreateThread(NULL, 0, client, NULL, 0, NULL);
    if (overlapped)
    {
        if (!ConnectNamedPipe(h, &ov) && GetLastError() == ERROR_IO_PENDING) GetOverlappedResult(h, &ov, &n, TRUE);
    }
    else ConnectNamedPipe(h, NULL);
    while (total < 100 + 150 + 200 + 250 + 300)
    {
        BOOL ok;
        memset(buf, 0xcc, sizeof(buf));
        if (overlapped)
        {
            ResetEvent(ov.hEvent);
            ok = ReadFile(h, buf, sizeof(buf), &n, &ov);
            if (!ok && GetLastError() == ERROR_IO_PENDING) ok = GetOverlappedResult(h, &ov, &n, TRUE);
        }
        else ok = ReadFile(h, buf, sizeof(buf), &n, NULL);
        if (!ok) { printf("read failed %lu\n", GetLastError()); break; }
        for (i = 1; i < n; i++) if (buf[i] != buf[0]) bad++;
        printf("  read %lu bytes of '%c'\n", n, buf[0]);
        total += n;
    }
    memset(buf, 'z', 10);
    WriteFile(h, buf, 10, &n, NULL);
    WaitForSingleObject(t, INFINITE);
    CloseHandle(h);
    printf("%s %s: total %lu, %lu bad bytes\n", type == PIPE_TYPE_MESSAGE ? "message" : "byte",
           overlapped ? "overlapped" : "blocking", total, bad);
    return bad;
}

int main(void)
{
    run(PIPE_TYPE_MESSAGE, FALSE);
    run(PIPE_TYPE_MESSAGE, TRUE);
    run(PIPE_TYPE_BYTE, FALSE);
    run(PIPE_TYPE_BYTE, TRUE);
    return 0;
}
