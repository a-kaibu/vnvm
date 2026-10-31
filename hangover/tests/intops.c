/* Integer and x87 instructions the JIT does with care (shifts by CL with the flags read after, FILD of
 * every size) over pseudo-random data, with a checksum per group, to compare the JIT (default) with the
 * interpreter (VNVM_JIT=0) or native Wine.
 * Build: i686-w64-mingw32-gcc -O2 intops.c -o intops.exe */
#include <stdio.h>

static unsigned seed = 12345;
static unsigned rnd(void)
{
    seed = seed * 1103515245 + 12345;
    return seed ^ (seed >> 15);
}

static unsigned sum;
static void mix(unsigned x)
{
    sum = (sum * 31) ^ x;
}

/* flags after a CMP (so a zero count shows they stay), then the shift; the status flags are pushed */
#define SHIFT(op, reg, cast)                                                                       \
    {                                                                                              \
        unsigned v = a, f;                                                                         \
        __asm__ volatile("cmp %3, %2\n\t" op " %%cl, %" reg "0\n\tpushf\n\tpop %1"                \
                         : "+q"(v), "=r"(f)                                                        \
                         : "r"(a), "r"(b), "c"(c)                                                  \
                         : "cc");                                                                  \
        mix((cast)v);                                                                              \
        mix(f & 0x8d5); /* OF SF ZF AF PF CF */                                                    \
    }

static unsigned shifts(void)
{
    sum = 0;
    for (int i = 0; i < 20000; i++)
    {
        unsigned a = rnd(), b = rnd(), c = rnd() % 40;
        if (i % 7 == 0) c = 0;
        if (i % 11 == 0) b = a;
        SHIFT("shll", "k", unsigned)
        SHIFT("shrl", "k", unsigned)
        SHIFT("sarl", "k", unsigned)
        SHIFT("shlw", "w", unsigned short)
        SHIFT("shrw", "w", unsigned short)
        SHIFT("sarw", "w", unsigned short)
        SHIFT("shlb", "b", unsigned char)
        SHIFT("shrb", "b", unsigned char)
        SHIFT("sarb", "b", unsigned char)
    }
    return sum;
}

static unsigned filds(void)
{
    sum = 0;
    for (int i = 0; i < 20000; i++)
    {
        short s = (short)rnd();
        int w = (int)rnd();
        long long q = ((long long)rnd() << 32 | rnd()) >> (rnd() % 40);
        long long r16, r32, r64;
        double d;
        __asm__ volatile("filds %1\n\tfistpll %0" : "=m"(r16) : "m"(s));
        __asm__ volatile("fildl %1\n\tfistpll %0" : "=m"(r32) : "m"(w));
        __asm__ volatile("fildll %1\n\tfistpll %0" : "=m"(r64) : "m"(q));
        __asm__ volatile("fildll %1\n\tfildl %2\n\tfaddp\n\tfstpl %0" : "=m"(d) : "m"(q), "m"(w));
        mix((unsigned)r16);
        mix((unsigned)r32);
        mix((unsigned)r64 ^ (unsigned)(r64 >> 32));
        unsigned long long x;
        __builtin_memcpy(&x, &d, 8);
        mix((unsigned)x ^ (unsigned)(x >> 32));
    }
    return sum;
}

/* SHLD/SHRD by CL and by an immediate, with and without the flags read after */
static unsigned dshifts(void)
{
    sum = 0;
    for (int i = 0; i < 20000; i++)
    {
        unsigned a = rnd(), b = rnd(), c = rnd() % 40, x, y, f;
        x = a;
        __asm__ volatile("shldl %%cl, %2, %0" : "+r"(x) : "c"(c), "r"(b) : "cc");
        y = a;
        __asm__ volatile("shrdl %%cl, %2, %0" : "+r"(y) : "c"(c), "r"(b) : "cc");
        mix(x);
        mix(y);
        x = a;
        __asm__ volatile("shldl $7, %1, %0" : "+r"(x) : "r"(b) : "cc");
        y = a;
        __asm__ volatile("shrdl $13, %1, %0" : "+r"(y) : "r"(b) : "cc");
        mix(x);
        mix(y);
        x = a;
        __asm__ volatile("cmp %3, %2\n\tshrdl %%cl, %4, %0\n\tpushf\n\tpop %1" : "+r"(x), "=r"(f) : "r"(a), "r"(b), "r"(b), "c"(c) : "cc");
        mix(x);
        mix(f & 0x8d5);
    }
    return sum;
}

/* MOVLPS/MOVHPS loads and stores of an XMM register's halves */
static unsigned halves(void)
{
    sum = 0;
    unsigned m[4], o[4], p[2];
    for (int i = 0; i < 20000; i++)
    {
        for (int k = 0; k < 4; k++) m[k] = rnd();
        p[0] = rnd();
        p[1] = rnd();
        __asm__ volatile("movups %2, %%xmm1\n\tmovlps %3, %%xmm1\n\tmovhps %3, %%xmm2\n\tmovups %%xmm1, %0\n\t"
                         "movhps %%xmm1, %1"
                         : "=m"(o), "=m"(p)
                         : "m"(m), "m"(p)
                         : "xmm1", "xmm2");
        for (int k = 0; k < 4; k++) mix(o[k]);
        mix(p[0]);
        mix(p[1]);
    }
    return sum;
}

int main(void)
{
    printf("shifts %08x\n", shifts());
    printf("filds  %08x\n", filds());
    printf("dshift %08x\n", dshifts());
    printf("halves %08x\n", halves());
    return 0;
}
