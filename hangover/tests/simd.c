/* MMX / SSE2 integer and SSE float instructions over pseudo-random data, with a checksum per group, to
 * compare the JIT (default) with the interpreter (VNVM_JIT=0) or native Wine.
 * Build: i686-w64-mingw32-gcc -O2 -mmmx -msse2 simd.c -o simd.exe */
#include <stdio.h>
#include <string.h>
#include <mmintrin.h>
#include <emmintrin.h>
#include <xmmintrin.h>

static unsigned seed = 12345;
static unsigned rnd(void)
{
    seed = seed * 1103515245 + 12345;
    return seed ^ (seed >> 15);
}

static unsigned sum;
static void mix64(__m64 v)
{
    unsigned long long x;
    memcpy(&x, &v, 8);
    sum = (sum * 31) ^ (unsigned)x ^ (unsigned)(x >> 32) * 7;
}
static void mix128(__m128i v)
{
    unsigned x[4];
    _mm_storeu_si128((__m128i *)x, v);
    sum = (sum * 31) ^ x[0] ^ x[1] * 3 ^ x[2] * 5 ^ x[3] * 7;
}

#define N 20000

static __attribute__((noinline)) unsigned mmx_arith(void)
{
    sum = 0;
    for (int i = 0; i < N; i++)
    {
        __m64 a = _mm_set_pi32(rnd(), rnd()), b = _mm_set_pi32(rnd(), rnd());
        mix64(_mm_add_pi8(a, b)); mix64(_mm_add_pi16(a, b)); mix64(_mm_add_pi32(a, b));
        mix64(_mm_sub_pi8(a, b)); mix64(_mm_sub_pi16(a, b)); mix64(_mm_sub_pi32(a, b));
        mix64(_mm_adds_pi8(a, b)); mix64(_mm_adds_pu8(a, b)); mix64(_mm_adds_pi16(a, b)); mix64(_mm_adds_pu16(a, b));
        mix64(_mm_subs_pi8(a, b)); mix64(_mm_subs_pu8(a, b)); mix64(_mm_subs_pi16(a, b)); mix64(_mm_subs_pu16(a, b));
        mix64(_mm_mullo_pi16(a, b)); mix64(_mm_mulhi_pi16(a, b)); mix64(_mm_madd_pi16(a, b));
        mix64(_mm_and_si64(a, b)); mix64(_mm_andnot_si64(a, b)); mix64(_mm_or_si64(a, b)); mix64(_mm_xor_si64(a, b));
        mix64(_mm_cmpeq_pi8(a, a)); mix64(_mm_cmpgt_pi8(a, b)); mix64(_mm_cmpgt_pi16(a, b)); mix64(_mm_cmpgt_pi32(a, b));
        mix64(_mm_cmpeq_pi16(a, b)); mix64(_mm_cmpeq_pi32(a, b));
    }
    _mm_empty();
    return sum;
}

static __attribute__((noinline)) unsigned mmx_shuffle(void)
{
    sum = 0;
    for (int i = 0; i < N; i++)
    {
        __m64 a = _mm_set_pi32(rnd(), rnd()), b = _mm_set_pi32(rnd(), rnd());
        __m64 c = _mm_cvtsi32_si64(rnd() & 63);
        mix64(_mm_unpacklo_pi8(a, b)); mix64(_mm_unpackhi_pi8(a, b));
        mix64(_mm_unpacklo_pi16(a, b)); mix64(_mm_unpackhi_pi16(a, b));
        mix64(_mm_unpacklo_pi32(a, b)); mix64(_mm_unpackhi_pi32(a, b));
        mix64(_mm_packs_pi16(a, b)); mix64(_mm_packs_pu16(a, b)); mix64(_mm_packs_pi32(a, b));
        mix64(_mm_slli_pi16(a, 3)); mix64(_mm_srli_pi16(a, 5)); mix64(_mm_srai_pi16(a, 7));
        mix64(_mm_slli_pi32(a, 9)); mix64(_mm_srli_pi32(a, 17)); mix64(_mm_srai_pi32(a, 31));
        mix64(_mm_slli_si64(a, 13)); mix64(_mm_srli_si64(a, 40));
        mix64(_mm_sll_pi16(a, c)); mix64(_mm_srl_pi32(a, c)); mix64(_mm_sra_pi16(a, c)); mix64(_mm_sra_pi32(a, c));
        mix64(_mm_sll_si64(a, c)); mix64(_mm_srl_si64(b, c));
        sum ^= _mm_cvtsi64_si32(a) + _mm_movemask_pi8(b);
        mix64(_mm_shuffle_pi16(a, 0x1b)); mix64(_mm_avg_pu8(a, b)); mix64(_mm_avg_pu16(a, b));
        mix64(_mm_min_pu8(a, b)); mix64(_mm_max_pi16(a, b)); mix64(_mm_mulhi_pu16(a, b));
    }
    _mm_empty();
    return sum;
}

static __attribute__((noinline)) unsigned sse2_int(void)
{
    sum = 0;
    for (int i = 0; i < N; i++)
    {
        __m128i a = _mm_set_epi32(rnd(), rnd(), rnd(), rnd()), b = _mm_set_epi32(rnd(), rnd(), rnd(), rnd());
        __m128i c = _mm_cvtsi32_si128(rnd() & 63);
        mix128(_mm_add_epi8(a, b)); mix128(_mm_adds_epu16(a, b)); mix128(_mm_sub_epi64(a, b));
        mix128(_mm_mullo_epi16(a, b)); mix128(_mm_mulhi_epi16(a, b)); mix128(_mm_mulhi_epu16(a, b)); mix128(_mm_madd_epi16(a, b));
        mix128(_mm_unpacklo_epi8(a, b)); mix128(_mm_unpackhi_epi16(a, b)); mix128(_mm_unpackhi_epi64(a, b));
        mix128(_mm_packus_epi16(a, b)); mix128(_mm_packs_epi32(a, b));
        mix128(_mm_srai_epi16(a, 3)); mix128(_mm_srl_epi32(a, c)); mix128(_mm_sll_epi64(a, c)); mix128(_mm_sra_epi32(b, c));
        mix128(_mm_slli_si128(a, 5)); mix128(_mm_srli_si128(b, 11));
        mix128(_mm_shuffle_epi32(a, 0x9c)); mix128(_mm_cmpgt_epi16(a, b)); mix128(_mm_avg_epu8(a, b));
        mix128(_mm_andnot_si128(a, b));
        sum ^= _mm_movemask_epi8(a) + _mm_cvtsi128_si32(b);
        mix128(_mm_loadl_epi64(&a));
    }
    return sum;
}

/* floats from the random bits, NaNs and infinities included now and then */
static float rf(void)
{
    unsigned r = rnd();
    if ((r & 63) == 0) return (r & 64) ? __builtin_nanf("") : __builtin_inff();
    return (float)(int)(r >> 8) / (float)((r & 0xff) + 1) - 1000.0f;
}
static void mixps(__m128 v)
{
    mix128(_mm_castps_si128(v));
}

static __attribute__((noinline)) unsigned sse_float(void)
{
    sum = 0;
    static float mem[8];
    for (int i = 0; i < N; i++)
    {
        __m128 a = _mm_set_ps(rf(), rf(), rf(), rf()), b = _mm_set_ps(rf(), rf(), rf(), rf());
        for (int k = 0; k < 8; k++) mem[k] = rf();
        __m128 m = _mm_loadu_ps(mem);
        mixps(_mm_add_ps(a, b)); mixps(_mm_sub_ps(a, b)); mixps(_mm_mul_ps(a, b)); mixps(_mm_div_ps(a, b));
        mixps(_mm_min_ps(a, b)); mixps(_mm_max_ps(a, b)); mixps(_mm_sqrt_ps(_mm_mul_ps(a, a)));
        mixps(_mm_add_ps(a, m)); mixps(_mm_mul_ps(m, b));
        mixps(_mm_add_ss(a, b)); mixps(_mm_sub_ss(a, b)); mixps(_mm_mul_ss(a, b)); mixps(_mm_div_ss(a, b));
        mixps(_mm_sqrt_ss(_mm_mul_ss(b, b)));
        mixps(_mm_add_ss(a, _mm_load_ss(mem + 3))); mixps(_mm_move_ss(a, b)); mixps(_mm_load_ss(mem + 5));
        _mm_store_ss(mem + 1, _mm_mul_ss(a, b)); sum ^= *(unsigned *)(mem + 1);
        mixps(_mm_cmpeq_ps(a, b)); mixps(_mm_cmplt_ps(a, b)); mixps(_mm_cmple_ps(a, b)); mixps(_mm_cmpunord_ps(a, b));
        mixps(_mm_cmpneq_ps(a, b)); mixps(_mm_cmpnlt_ps(a, b)); mixps(_mm_cmpnle_ps(a, b)); mixps(_mm_cmpord_ps(a, b));
        mixps(_mm_shuffle_ps(a, b, 0x1b)); mixps(_mm_shuffle_ps(a, b, 0xe4)); mixps(_mm_shuffle_ps(a, m, 0x93));
        mixps(_mm_movehl_ps(a, b)); mixps(_mm_movelh_ps(a, b));
        mixps(_mm_unpacklo_ps(a, b)); mixps(_mm_unpackhi_ps(a, b));
        __m128d c = _mm_castps_pd(a), d = _mm_set_pd(rf(), rf());
        mixps(_mm_castpd_ps(_mm_add_pd(c, d))); mixps(_mm_castpd_ps(_mm_mul_pd(c, d))); mixps(_mm_castpd_ps(_mm_div_pd(d, c)));
        mixps(_mm_castpd_ps(_mm_min_pd(c, d))); mixps(_mm_castpd_ps(_mm_max_pd(c, d)));
        mixps(_mm_castpd_ps(_mm_add_sd(c, d))); mixps(_mm_castpd_ps(_mm_sqrt_sd(c, d)));
    }
    return sum;
}

int main(void)
{
    printf("mmx_arith   %08x\n", mmx_arith());
    printf("mmx_shuffle %08x\n", mmx_shuffle());
    printf("sse2_int    %08x\n", sse2_int());
    printf("sse_float   %08x\n", sse_float());
    return 0;
}
