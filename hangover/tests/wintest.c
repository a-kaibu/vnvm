/* window smoke test: a window with coloured rectangles, closes itself after a few seconds */
#include <windows.h>

static LRESULT CALLBACK wndproc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    switch (msg)
    {
    case WM_PAINT:
    {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(hwnd, &ps);
        RECT rc;
        int i;
        GetClientRect(hwnd, &rc);
        for (i = 0; i < 8; i++)
        {
            HBRUSH b = CreateSolidBrush(RGB(i & 1 ? 255 : 0, i & 2 ? 255 : 0, i & 4 ? 255 : 0));
            RECT r = { rc.right * i / 8, 0, rc.right * (i + 1) / 8, rc.bottom / 2 };
            FillRect(dc, &r, b);
            DeleteObject(b);
        }
        Ellipse(dc, rc.right / 4, rc.bottom / 2 + 10, rc.right * 3 / 4, rc.bottom - 10);
        EndPaint(hwnd, &ps);
        return 0;
    }
    case WM_LBUTTONDOWN:
        MessageBeep(0);
        SetWindowTextA(hwnd, "clicked");
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
    WNDCLASSA wc = { 0, wndproc, 0, 0, inst, 0, LoadCursor(0, IDC_ARROW), (HBRUSH)(COLOR_WINDOW + 1), 0, "vnvmtest" };
    HWND hwnd;
    MSG msg;
    RegisterClassA(&wc);
    hwnd = CreateWindowA("vnvmtest", "vnvm test", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 80, 640, 400, 0, 0, inst, 0);
    SetTimer(hwnd, 1, atoi(cmd) > 0 ? atoi(cmd) : 8000, NULL);
    while (GetMessageA(&msg, 0, 0, 0)) { TranslateMessage(&msg); DispatchMessageA(&msg); }
    return 0;
}
