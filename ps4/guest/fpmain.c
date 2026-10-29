/* Floating-point guest: scalar and vectorisable float/double code as a game has, printed
 * as bit patterns (no float formatting in libmini). No fast-math, no contraction: the
 * results are fixed by IEEE semantics, so every build must print the native run's bits. */
typedef unsigned long u64;
typedef unsigned int u32;

void puts_n(const char *);
void put_num(long, int, int);
void exit(int) __attribute__((noreturn));

static u32 fbits(float f)
{
    union { float f; u32 u; } v = { f };
    return v.u;
}

static u64 dbits(double d)
{
    union { double d; u64 u; } v = { d };
    return v.u;
}

static void hex(const char *name, u64 v)
{
    puts_n(name);
    puts_n(" 0x");
    put_num((long)v, 16, 0);
    puts_n("\n");
}

static u32 get_mxcsr(void)
{
    u32 m;
    __asm__ volatile("stmxcsr %0" : "=m"(m) : : "memory");
    return m;
}

static void set_mxcsr(u32 m)
{
    /* "memory": the float work around it must stay on its side. */
    __asm__ volatile("ldmxcsr %0" : : "m"(m) : "memory");
}

/* Leibniz series for pi in float and double. */
static void t_series(void)
{
    float fs = 0;
    double ds = 0;
    for (int k = 0; k < 2000; k++) {
        float term = 1.0f / (2 * k + 1);
        fs += k & 1 ? -term : term;
        ds += (k & 1 ? -1.0 : 1.0) / (2.0 * k + 1);
    }
    hex("pi.f", fbits(4 * fs));
    hex("pi.d", dbits(4 * ds));
}

/* 4x4 matrix products, as a renderer chains transforms. */
static void t_matrix(void)
{
    float a[16], b[16], c[16];
    for (int i = 0; i < 16; i++) {
        a[i] = (float)(i * 3 % 7) * 0.25f - 0.6f;
        b[i] = (float)(i * 5 % 11) * 0.125f + 0.3f;
    }
    for (int round = 0; round < 8; round++) {
        for (int r = 0; r < 4; r++)
            for (int col = 0; col < 4; col++) {
                float s = 0;
                for (int k = 0; k < 4; k++)
                    s += a[r * 4 + k] * b[k * 4 + col];
                c[r * 4 + col] = s;
            }
        for (int i = 0; i < 16; i++)
            a[i] = c[i] * 0.5f;
    }
    u32 h = 0;
    for (int i = 0; i < 16; i++)
        h = h * 31 + fbits(c[i]);
    hex("matrix", h);
}

/* Vector normalisation with sqrt and a division per component. */
static void t_normalize(void)
{
    u32 h = 0;
    for (int i = 1; i < 200; i++) {
        float x = i * 0.37f, y = 1.0f / i, z = -i * 0.011f;
        float len = __builtin_sqrtf(x * x + y * y + z * z);
        h = h * 33 + fbits(x / len) + fbits(y / len) + fbits(z / len);
    }
    hex("normalize", h);
    double d = 2.0;
    for (int i = 0; i < 5; i++)
        d = 0.5 * (d + 2.0 / d); /* Newton's sqrt(2) */
    hex("newton", dbits(d));
    hex("sqrt.d", dbits(__builtin_sqrt(3.0)));
}

/* Conversions, truncation, clamps written as compares. */
static void t_convert(void)
{
    long acc = 0;
    u64 h = 0;
    for (int i = -300; i < 300; i += 7) {
        float f = i * 1.37f;
        double d = i * -2.71828;
        acc += (int)f + (long)d + (int)(f * 1000.0f);
        float clamped = f < -100.0f ? -100.0f : (f > 100.0f ? 100.0f : f);
        h = h * 7 + fbits(clamped) + dbits((double)f) + fbits((float)d);
        h += fbits((float)i) + dbits((double)(long)i * 1e10);
    }
    hex("convert.i", (u64)acc);
    hex("convert.f", h);
}

/* Specials: infinities, NaN propagation through arithmetic, signed zeros. */
static void t_specials(void)
{
    volatile float zero = 0.0f, one = 1.0f, big = 3.0e38f;
    float inf = one / zero;
    float nan = zero / zero;
    hex("inf", fbits(inf));
    hex("nan", fbits(nan));
    hex("inf-inf", fbits(inf - inf));
    hex("overflow", fbits(big * 10.0f));
    hex("negzero", fbits(-zero * one));
    hex("nan+1", fbits(nan + one));
    hex("cmp", (u64)((nan < one) + 2 * (nan == nan) + 4 * (inf > big) + 8 * (zero == -zero)));
}

/* Denormals: the default MXCSR keeps them; with FTZ and DAZ on (as inFAMOUS sets in its
 * job threads) results and inputs below 2^-126 become zero. */
static void t_denormals(void)
{
    volatile float tiny = 1.0e-38f, scale = 0.001f, denorm = 1.0e-40f;
    hex("denorm.keep", fbits(tiny * scale));
    hex("denorm.in", fbits(denorm * 1000.0f));
    u32 saved = get_mxcsr();
    set_mxcsr(saved | 0x8040);
    volatile float ftz = tiny * scale;
    volatile float daz = denorm * 1000.0f;
    u32 flags = get_mxcsr() & 0x3f;
    set_mxcsr(saved);
    hex("denorm.ftz", fbits(ftz));
    hex("denorm.daz", fbits(daz));
    hex("denorm.flags", flags);
    volatile double dt = 1.0e-308, ds = 1.0e-10;
    hex("denorm.d", dbits(dt * ds));
}

/* Round toward zero and toward +inf through MXCSR.RC. */
static void t_rounding(void)
{
    volatile float a = 1.0f, b = 3.0f;
    u32 saved = get_mxcsr();
    u32 r = 0;
    for (u32 rc = 0; rc < 4; rc++) {
        set_mxcsr((saved & ~0x6000u) | rc << 13);
        volatile float q = a / b, nq = -a / b;
        r = r * 0x10001 + fbits(q) + fbits(nq);
    }
    set_mxcsr(saved);
    hex("rounding", r);
}

/* A particle step: positions, velocities, damping, as a vectorisable loop. */
static void t_particles(void)
{
    float px[64], vx[64];
    for (int i = 0; i < 64; i++) {
        px[i] = i * 0.5f;
        vx[i] = (i % 7) - 3.0f;
    }
    for (int step = 0; step < 50; step++)
        for (int i = 0; i < 64; i++) {
            vx[i] = vx[i] * 0.98f - px[i] * 0.01f;
            px[i] = px[i] + vx[i] * 0.016f;
        }
    u32 h = 0;
    for (int i = 0; i < 64; i++)
        h = h * 31 + fbits(px[i]);
    hex("particles", h);
}

void _start(void)
{
    puts_n("fpguest: start\n");
    t_series();
    t_matrix();
    t_normalize();
    t_convert();
    t_specials();
    t_denormals();
    t_rounding();
    t_particles();
    hex("mxcsr", get_mxcsr() & ~0x3fu);
    puts_n("fpguest: done\n");
    exit(5);
}
