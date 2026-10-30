/* Logs the mouse and key messages a window gets (for checking input delivery under vnvm:
 * VNVM_INPUT clicks at known places, count what arrives). Quits after 120 s.
 * Build: i686-w64-mingw32-gcc -O2 clicktest.c -o clicktest.exe -mwindows */
#include <windows.h>
#include <stdio.h>

static LRESULT CALLBACK wndproc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    switch (msg)
    {
    case WM_LBUTTONDOWN:
    case WM_LBUTTONUP:
    case WM_MOUSEMOVE:
        printf("%lu %s %d,%d\n", GetTickCount(), msg == WM_LBUTTONDOWN ? "down" : msg == WM_LBUTTONUP ? "up" : "move",
               (short)LOWORD(lp), (short)HIWORD(lp));
        fflush(stdout);
        return 0;
    case WM_KEYDOWN:
        printf("%lu key %u\n", GetTickCount(), (unsigned)wp);
        fflush(stdout);
        return 0;
    case WM_TIMER:
        DestroyWindow(hwnd);
        return 0;
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcA(hwnd, msg, wp, lp);
}

int WINAPI WinMain(HINSTANCE inst, HINSTANCE prev, LPSTR cmd, int show)
{
    WNDCLASSA wc = { 0, wndproc, 0, 0, inst, 0, LoadCursor(0, IDC_ARROW), (HBRUSH)(COLOR_WINDOW + 1), 0, "clicktest" };
    HWND hwnd;
    MSG msg;
    RegisterClassA(&wc);
    hwnd = CreateWindowA("clicktest", "clicktest", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 80, 640, 400, 0, 0, inst, 0);
    SetTimer(hwnd, 1, 120000, NULL);
    while (GetMessageA(&msg, 0, 0, 0))
    {
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }
    return 0;
}
