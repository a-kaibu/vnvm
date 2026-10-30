/* CPU benchmark for the x86 core: a few kernels typical of VN engines (decompression-like
 * byte loops, memcpy/blits, integer math, sorting, x87/SSE float), timed with
 * QueryPerformanceCounter. Prints "name ms" per kernel and a checksum so runs can be compared
 * against native Wine. Optional arguments: repeat count scale (default 1), and the name of the
 * one kernel to run (5 times) instead of all.
 * Build: i686-w64-mingw32-gcc -O2 bench.c -o bench.exe */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static LARGE_INTEGER freq;
static unsigned scale = 1;

static double now_ms(void)
{
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return t.QuadPart * 1000.0 / freq.QuadPart;
}

static unsigned crc_table[256];

static unsigned k_crc32(unsigned char *buf, unsigned len)
{
    unsigned i, r, c = ~0u;
    for (r = 0; r < 20 * scale; r++)
        for (i = 0; i < len; i++) c = crc_table[(c ^ buf[i]) & 0xff] ^ (c >> 8);
    return ~c;
}

/* LZSS-style decoder over a synthetic stream (byte-at-a-time control flow) */
static unsigned k_lz(unsigned char *src, unsigned len, unsigned char *dst)
{
    unsigned r, sum = 0;
    for (r = 0; r < 20 * scale; r++)
    {
        unsigned s = 0, d = 0, flags = 0, bit = 0;
        while (s + 3 < len && d < len)
        {
            if (!bit) { flags = src[s++]; bit = 8; }
            if (flags & 1) dst[d++] = src[s++];
            else
            {
                unsigned off = (src[s] | (src[s + 1] & 0xf0) << 4) % (d + 1), n = (src[s + 1] & 15) + 3, k;
                s += 2;
                for (k = 0; k < n && d < len; k++, d++) dst[d] = dst[d - off - (d > off ? 1 : 0)];
            }
            flags >>= 1;
            bit--;
        }
        sum += d + dst[d / 2];
    }
    return sum;
}

/* 32bpp alpha blend of a 256x256 sprite onto a 640x480 surface */
static unsigned k_blend(unsigned *dst, const unsigned *spr)
{
    unsigned r, x, y;
    for (r = 0; r < 40 * scale; r++)
        for (y = 0; y < 256; y++)
            for (x = 0; x < 256; x++)
            {
                unsigned s = spr[y * 256 + x], a = s >> 24, *p = &dst[(y + r % 200) * 640 + x + r % 300];
                unsigned d = *p;
                unsigned rb = ((s & 0xff00ff) * a + (d & 0xff00ff) * (255 - a)) >> 8 & 0xff00ff;
                unsigned g = ((s & 0xff00) * a + (d & 0xff00) * (255 - a)) >> 8 & 0xff00;
                *p = rb | g | 0xff000000;
            }
    for (x = 0, y = 0; x < 640 * 480; x += 97) y += dst[x];
    return y;
}

static unsigned k_memcpy(unsigned char *a, unsigned char *b, unsigned len)
{
    unsigned r;
    for (r = 0; r < 200 * scale; r++)
    {
        memcpy(a, b, len);
        memmove(b + 1, a, len - 1);
    }
    return a[len / 3] + b[len / 2];
}

static int cmp(const void *a, const void *b)
{
    int x = *(const int *)a, y = *(const int *)b;
    return x < y ? -1 : x > y;
}

static unsigned k_sort(int *v, unsigned n)
{
    unsigned r, i, seed = 1, sum = 0;
    for (r = 0; r < 4 * scale; r++)
    {
        for (i = 0; i < n; i++) v[i] = (seed = seed * 1103515245 + 12345) >> 1;
        qsort(v, n, sizeof(*v), cmp);
        sum += v[n / 2];
    }
    return sum;
}

static unsigned k_float(void)
{
    double x = 0.5, acc = 0;
    float m[16], v[4] = { 1, 2, 3, 4 }, o[4];
    unsigned r, i, j;
    for (i = 0; i < 16; i++) m[i] = (float)(i * 0.25 - 1.5);
    for (r = 0; r < 2000000 * scale; r++)
    {
        x = x * 1.0000001 + 0.000001;
        acc += x / (1.0 + x * x);
        if ((r & 7) == 0)
        {
            for (i = 0; i < 4; i++)
            {
                float s = 0;
                for (j = 0; j < 4; j++) s += m[i * 4 + j] * v[j];
                o[i] = s;
            }
            for (i = 0; i < 4; i++) v[i] = o[i] * 0.25f;
        }
    }
    return (unsigned)(acc * 1000) + (unsigned)(v[0] * 1000);
}

static unsigned k_calls(unsigned n);
static unsigned __attribute__((noinline)) leaf(unsigned a, unsigned b) { return (a ^ b) * 2654435761u >> 3; }
static unsigned k_calls(unsigned n)
{
    unsigned i, s = 0;
    for (i = 0; i < n * scale; i++) s += leaf(s, i);
    return s;
}

int main(int argc, char **argv)
{
    static unsigned char buf[1 << 20], out[1 << 20], big[4 << 20], big2[4 << 20];
    static unsigned surf[640 * 480], spr[256 * 256];
    static int sortv[200000];
    unsigned i, sum = 0, crc;
    const char *only = NULL;
    double t, total = 0;

    if (argc > 1) scale = atoi(argv[1]) > 0 ? atoi(argv[1]) : 1;
    if (argc > 2) only = argv[2];
    QueryPerformanceFrequency(&freq);
    for (i = 0; i < 256; i++)
    {
        unsigned c = i, k;
        for (k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >> 1) : c >> 1;
        crc_table[i] = c;
    }
    for (i = 0; i < sizeof(buf); i++) buf[i] = (unsigned char)(i * 7 + (i >> 9));
    for (i = 0; i < sizeof(big2); i++) big2[i] = (unsigned char)(i * 13 + (i >> 11));
    for (i = 0; i < 256 * 256; i++) spr[i] = (i * 2654435761u) | ((i & 255) << 24);

#define RUN(name, expr) \
    for (i = 0; i < (only ? 5 : 1); i++) if (!only || !strcmp(only, name)) { \
    t = now_ms(); crc = (expr); t = now_ms() - t; total += t; sum += crc; \
    printf("%-8s %8.1f ms  %08x\n", name, t, crc); fflush(stdout); }

    RUN("crc32", k_crc32(buf, sizeof(buf)));
    RUN("lz", k_lz(buf, sizeof(buf), out));
    RUN("blend", k_blend(surf, spr));
    RUN("memcpy", k_memcpy(big, big2, sizeof(big)));
    RUN("sort", k_sort(sortv, 200000));
    RUN("float", k_float());
    RUN("calls", k_calls(20000000));
    printf("total    %8.1f ms  %08x\n", total, sum);
    return 0;
}
