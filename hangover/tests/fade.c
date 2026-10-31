/* KiriKiri's MMX crossfade inner loop (Reunion 0x654640) over a 1280x720 frame, timed: the
 * per-frame cost of a screen transition. Build: i686-w64-mingw32-gcc -O2 fade.c -o fade.exe */
#include <stdio.h>
#include <stdlib.h>
#include <windows.h>

static void blend(unsigned char *dst, const unsigned char *a, const unsigned char *b, unsigned n, unsigned opa)
{
    unsigned long long o = opa * 0x0001000100010001ull;
    __asm__ volatile(
        "pxor %%mm0, %%mm0\n\t"
        "movq %4, %%mm4\n\t"
        "xor %%ebx, %%ebx\n"
        "1:\n\t"
        "movq (%%esi,%%ebx), %%mm2\n\t"
        "movq 8(%%esi,%%ebx), %%mm3\n\t"
        "movq (%%eax,%%ebx), %%mm1\n\t"
        "movq 8(%%eax,%%ebx), %%mm7\n\t"
        "movq %%mm2, %%mm6\n\t"
        "movq %%mm1, %%mm5\n\t"
        "punpcklbw %%mm0, %%mm2\n\t"
        "punpcklbw %%mm0, %%mm1\n\t"
        "punpckhbw %%mm0, %%mm6\n\t"
        "punpckhbw %%mm0, %%mm5\n\t"
        "psubw %%mm1, %%mm2\n\t"
        "psubw %%mm5, %%mm6\n\t"
        "pmullw %%mm4, %%mm2\n\t"
        "psllw $8, %%mm1\n\t"
        "pmullw %%mm4, %%mm6\n\t"
        "psllw $8, %%mm5\n\t"
        "paddw %%mm2, %%mm1\n\t"
        "paddw %%mm6, %%mm5\n\t"
        "psrlw $8, %%mm1\n\t"
        "psrlw $8, %%mm5\n\t"
        "add $16, %%ebx\n\t"
        "packuswb %%mm5, %%mm1\n\t"
        "movq %%mm3, %%mm6\n\t"
        "movq %%mm7, %%mm5\n\t"
        "punpcklbw %%mm0, %%mm3\n\t"
        "punpckhbw %%mm0, %%mm6\n\t"
        "punpcklbw %%mm0, %%mm7\n\t"
        "punpckhbw %%mm0, %%mm5\n\t"
        "psubw %%mm7, %%mm3\n\t"
        "psubw %%mm5, %%mm6\n\t"
        "pmullw %%mm4, %%mm3\n\t"
        "psllw $8, %%mm7\n\t"
        "pmullw %%mm4, %%mm6\n\t"
        "psllw $8, %%mm5\n\t"
        "movq %%mm1, -16(%%edi,%%ebx)\n\t"
        "paddw %%mm3, %%mm7\n\t"
        "paddw %%mm6, %%mm5\n\t"
        "psrlw $8, %%mm7\n\t"
        "psrlw $8, %%mm5\n\t"
        "packuswb %%mm5, %%mm7\n\t"
        "movq %%mm7, -8(%%edi,%%ebx)\n\t"
        "cmp %3, %%ebx\n\t"
        "jl 1b\n\t"
        "emms"
        : : "S"(b), "a"(a), "D"(dst), "r"(n), "m"(o) : "ebx", "memory");
}

int main(void)
{
    unsigned n = 1280 * 720 * 4;
    unsigned char *a = malloc(n), *b = malloc(n), *d = malloc(n);
    LARGE_INTEGER f, t0, t1;
    unsigned sum = 0;
    for (unsigned i = 0; i < n; i++) a[i] = i * 7, b[i] = i * 13 + 5;
    QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&t0);
    for (int k = 0; k < 60; k++) blend(d, a, b, n, k * 4);
    QueryPerformanceCounter(&t1);
    for (unsigned i = 0; i < n; i += 97) sum = sum * 31 + d[i];
    printf("fade 1280x720: %.2f ms/frame, check %08x\n", (t1.QuadPart - t0.QuadPart) * 1000.0 / f.QuadPart / 60, sum);
    return 0;
}
