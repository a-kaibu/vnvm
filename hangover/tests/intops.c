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

/* CF read by SETC/SETNC (a condition, not EFLAGS) after shifts by an immediate and by CL, INC/DEC
 * (CF kept from the CMP before) and MUL/IMUL */
#define CARRY(insn, ...)                                                                           \
    {                                                                                              \
        unsigned v = a, lo, hi;                                                                    \
        unsigned char c1, c2;                                                                      \
        __asm__ volatile("cmp %5, %4\n\t" insn "\n\tsetc %1\n\tsetnc %2"                         \
                         : "+a"(v), "=q"(c1), "=q"(c2), "=d"(hi)                                   \
                         : "r"(a), "r"(b), "c"(c)                                                  \
                         : "cc");                                                                  \
        lo = v;                                                                                    \
        mix(lo);                                                                                   \
        mix(c1 | c2 << 1);                                                                         \
    }

static unsigned carries(void)
{
    sum = 0;
    for (int i = 0; i < 20000; i++)
    {
        unsigned a = rnd(), b = rnd(), c = rnd() % 32 + 1;
        if (i % 11 == 0) b = a;
        CARRY("shrl $1, %0")
        CARRY("shrl $7, %0")
        CARRY("shrl %%cl, %0")
        CARRY("sarl $3, %0")
        CARRY("sarl %%cl, %0")
        CARRY("shll $1, %0")
        CARRY("shll %%cl, %0")
        CARRY("incl %0")
        CARRY("decl %0")
        CARRY("mull %5")
        CARRY("imull %5")
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

/* FISTP m16/m32/m64 under each rounding mode (FLDCW/FNSTCW), out-of-range values, EMMS, CLD/STD */
static unsigned fists(void)
{
    sum = 0;
    unsigned short cw0, cw;
    __asm__ volatile("fnstcw %0" : "=m"(cw0));
    for (int i = 0; i < 20000; i++)
    {
        int k = rnd() % 4;
        double d = (double)(int)rnd() / (double)(1 << (rnd() % 20));
        if (i % 13 == 0) d = (int)d + 0.5;
        if (i % 17 == 0) d *= 1e15;
        short r16;
        int r32;
        long long r64;
        cw = (cw0 & ~0xc00) | k << 10;
        unsigned short got;
        __asm__ volatile("fldcw %4\n\tfldl %5\n\tfistps %0\n\tfldl %5\n\tfistl %1\n\tfistpl %1\n\tfldl %5\n\tfistpll %2\n\t"
                         "fnstcw %3\n\twait\n\tfldcw %6"
                         : "=m"(r16), "=m"(r32), "=m"(r64), "=m"(got)
                         : "m"(cw), "m"(d), "m"(cw0));
        mix((unsigned short)r16);
        mix((unsigned)r32);
        mix((unsigned)r64 ^ (unsigned)(r64 >> 32));
        mix(got);
        unsigned f;
        __asm__ volatile("std\n\tpushf\n\tpop %0\n\tcld\n\temms" : "=r"(f));
        mix(f & 0x400);
        __asm__ volatile("pushf\n\tpop %0" : "=r"(f));
        mix(f & 0x400);
    }
    return sum;
}

/* MOVS/STOS/LODS with and without REP (overlapping and backward runs too), one-operand MUL/IMUL */
static unsigned strings(void)
{
    sum = 0;
    static unsigned char buf[600];
    for (int i = 0; i < 4000; i++)
    {
        for (int k = 0; k < 600; k += 4) *(unsigned *)(buf + k) = rnd();
        unsigned n = rnd() % 100, s = rnd() % 200, d = rnd() % 200, v = rnd();
        void *S = buf + s, *D = buf + d + 200;
        unsigned c = n;
        switch (i % 10)
        {
        case 0: __asm__ volatile("rep movsb" : "+S"(S), "+D"(D), "+c"(c) : : "memory"); break;
        case 1: __asm__ volatile("rep movsl" : "+S"(S), "+D"(D), "+c"(c) : : "memory"); break;
        case 2: D = buf + s + 3; __asm__ volatile("rep movsb" : "+S"(S), "+D"(D), "+c"(c) : : "memory"); break;
        case 3: __asm__ volatile("rep stosb" : "+D"(D), "+c"(c) : "a"(v) : "memory"); break;
        case 4: __asm__ volatile("rep stosw" : "+D"(D), "+c"(c) : "a"(v) : "memory"); break;
        case 5: __asm__ volatile("rep stosl" : "+D"(D), "+c"(c) : "a"(v) : "memory"); break;
        case 6:
            S = buf + 300 + s;
            __asm__ volatile("std\n\trep movsw\n\tcld" : "+S"(S), "+D"(D), "+c"(c) : : "memory");
            break;
        case 8:
            /* a backward memmove: the destination a little above the source (and below it) */
            S = buf + 300 + s; D = buf + 300 + s + (int)(v % 12) - 4; c = n % 60;
            __asm__ volatile("std\n\trep movsl\n\tcld" : "+S"(S), "+D"(D), "+c"(c) : : "memory");
            if (v & 1) { S = buf + 303 + s; D = buf + 305 + s; c = n; __asm__ volatile("std\n\trep movsb\n\tcld" : "+S"(S), "+D"(D), "+c"(c) : : "memory"); }
            break;
        case 9:
            if (v & 1) __asm__ volatile("std\n\trep stosb\n\tcld" : "+D"(D), "+c"(c) : "a"(v) : "memory");
            else { c = n % 50; __asm__ volatile("std\n\trep stosl\n\tcld" : "+D"(D), "+c"(c) : "a"(v) : "memory"); }
            break;
        default:
            for (unsigned k = 0; k < n; k++)
            {
                unsigned a;
                __asm__ volatile("lodsw\n\tstosb\n\tmovsl\n\tlodsb\n\tstosl" : "=a"(a), "+S"(S), "+D"(D) : : "memory");
                mix(a);
            }
        }
        mix(c);
        mix((unsigned)((unsigned char *)S - buf));
        mix((unsigned)((unsigned char *)D - buf));
        for (int k = 0; k < 600; k += 4) mix(*(unsigned *)(buf + k));
        unsigned a = rnd(), b = rnd(), lo, hi, f;
        if (i % 5 == 0) b = 1;
        __asm__ volatile("mull %4\n\tpushf\n\tpop %2" : "=a"(lo), "=d"(hi), "=r"(f) : "a"(a), "r"(b) : "cc");
        mix(lo); mix(hi); mix(f & 0x801);
        __asm__ volatile("imull %4\n\tpushf\n\tpop %2" : "=a"(lo), "=d"(hi), "=r"(f) : "a"(a), "m"(b) : "cc");
        mix(lo); mix(hi); mix(f & 0x801);
    }
    return sum;
}

/* locked read-modify-writes (aligned and not) with their flags */
#define LOCKED(ins, ...)                                                                           \
    {                                                                                              \
        unsigned f;                                                                                \
        __asm__ volatile(ins "\n\tpushf\n\tpop %0" : "=r"(f), __VA_ARGS__ : "cc", "memory");      \
        mix(f & 0x8d5);                                                                            \
    }

static unsigned locked(void)
{
    sum = 0;
    static unsigned char buf[16] __attribute__((aligned(16)));
    for (int i = 0; i < 20000; i++)
    {
        unsigned *p = (unsigned *)(buf + (i % 9 == 0 ? 1 : 4));
        unsigned v = rnd(), w = rnd(), e;
        *p = (i % 5 == 0) ? 1 : rnd();
        if (i % 7 == 0) v = -*p;
        LOCKED("lock addl %2, %1", "+m"(*p) : "r"(v))
        mix(*p);
        LOCKED("lock subl $3, %1", "+m"(*p) :)
        LOCKED("lock incl %1", "+m"(*p) :)
        LOCKED("lock decl %1", "+m"(*p) :)
        mix(*p);
        e = v;
        LOCKED("lock xaddl %2, %1", "+m"(*p), "+r"(e) :)
        mix(e);
        LOCKED("lock andl %2, %1", "+m"(*p) : "r"(w))
        LOCKED("lock orl $0x40, %1", "+m"(*p) :)
        LOCKED("lock xorl %2, %1", "+m"(*p) : "r"(v))
        mix(*p);
        e = (i & 1) ? *p : w;
        LOCKED("lock cmpxchgl %3, %1", "+m"(*p), "+a"(e) : "r"(v))
        mix(e);
        mix(*p);
        e = w;
        __asm__ volatile("xchgl %0, %1" : "+r"(e), "+m"(*p));
        mix(e);
        mix(*p);
    }
    return sum;
}

int main(void)
{
    printf("shifts %08x\n", shifts());
    printf("carry  %08x\n", carries());
    printf("filds  %08x\n", filds());
    printf("dshift %08x\n", dshifts());
    printf("halves %08x\n", halves());
    printf("fists  %08x\n", fists());
    printf("string %08x\n", strings());
    printf("locked %08x\n", locked());
    return 0;
}
