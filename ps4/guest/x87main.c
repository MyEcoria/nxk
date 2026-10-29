/* x87 guest: long double arithmetic, conversions, truncation through the control word,
 * fprem loops and precision control, printed as 10-byte hex patterns. */
typedef unsigned long u64;
typedef unsigned short u16;

void puts_n(const char *);
void put_num(long, int, int);
void exit(int) __attribute__((noreturn));

static void ldhex(const char *name, long double v)
{
    union { long double d; struct { u64 m; u16 e; } p; } u;
    u.d = 0; /* clear the padding */
    u.d = v;
    puts_n(name);
    puts_n(" 0x");
    put_num(u.p.e, 16, 0);
    puts_n(":");
    put_num((long)u.p.m, 16, 0);
    puts_n("\n");
}

static void hex(const char *name, u64 v)
{
    puts_n(name);
    puts_n(" 0x");
    put_num((long)v, 16, 0);
    puts_n("\n");
}

static u16 get_cw(void)
{
    u16 c;
    __asm__ volatile("fnstcw %0" : "=m"(c) : : "memory");
    return c;
}

static void set_cw(u16 c)
{
    __asm__ volatile("fldcw %0" : : "m"(c) : "memory");
}

/* x - n * y by repeated fprem: what libm's fmod does. */
static long double rem(long double x, long double y)
{
    long double r = x;
    __asm__("1: fprem; fnstsw %%ax; testb $4, %%ah; jnz 1b" : "+t"(r) : "u"(y) : "ax", "cc");
    return r;
}

static void t_series(void)
{
    long double s = 0, f = 1;
    for (int k = 1; k < 30; k++) {
        f /= k;
        s += (k & 1 ? f : -f);
    }
    ldhex("series", s);
    long double e = 1.0L, t = 1.0L;
    for (int k = 1; k < 40; k++) {
        t = t * 0.5L / k;
        e += t;
    }
    ldhex("exp.half", e);
}

static void t_convert(void)
{
    volatile double d = 3.14159265358979;
    volatile float f = 2.71828f;
    volatile long long i = -123456789012345LL;
    volatile long double l = d;
    ldhex("from.d", l);
    l = f;
    ldhex("from.f", l);
    l = i;
    ldhex("from.i", l);
    l = 1.0L / 3.0L;
    hex("to.d", ((union { double d; u64 u; }){ (double)l }).u);
    hex("to.f", ((union { float f; unsigned u; }){ (float)l }).u);
    volatile long double big = 9.0e18L;
    hex("to.i64", (u64)(long long)big);
    volatile long double neg = -2.5L;
    hex("trunc", (u64)(long long)neg); /* gcc: fnstcw / fldcw to truncate */
}

/* a / b as an x87 division the compiler cannot move across the control word changes. */
static long double div_asm(long double a, long double b)
{
    long double r;
    __asm__ volatile("fdivrp" : "=t"(r) : "0"(b), "u"(a) : "st(1)");
    return r;
}

static void t_precision(void)
{
    u16 saved = get_cw();
    volatile long double one = 1.0L, three = 3.0L;
    set_cw((saved & ~0x300) | 0x000); /* 24 bits */
    long double a = div_asm(one, three);
    __asm__ volatile("" : "+t"(a));
    set_cw((saved & ~0x300) | 0x200); /* 53 bits */
    long double b = div_asm(one, three);
    __asm__ volatile("" : "+t"(b));
    set_cw(saved);
    long double c = div_asm(one, three);
    ldhex("prec24", a);
    ldhex("prec53", b);
    ldhex("prec64", c);
    hex("cw", get_cw());
}

static void t_remainder(void)
{
    ldhex("rem.1", rem(10.0L, 3.0L));
    ldhex("rem.2", rem(-7.5L, 2.0L));
    ldhex("rem.3", rem(1.0e30L, 7.0L));
    ldhex("rem.4", rem(1.0e4000L, 1.0e-3L));
    ldhex("rem.5", rem(0.1L, 1.0e-4000L));
}

static void t_misc(void)
{
    volatile long double x = 2.0L, y = -0.0L, big = 1.0e4000L;
    ldhex("sqrt", __builtin_sqrtl(x));
    ldhex("fabs", __builtin_fabsl(-x));
    ldhex("negzero", y);
    ldhex("overflow", big * big);
    ldhex("inf-inf", (big * big) - (big * big));
    hex("cmp", (x < big) + 2 * (x == x) + 4 * (y == 0.0L) + 8 * (big > x));
    ldhex("sqrt.big", __builtin_sqrtl(big));
    long double acc = 0;
    for (int i = 1; i < 200; i++)
        acc += __builtin_sqrtl((long double)i) / (i + 0.5L);
    ldhex("acc", acc);
}

void _start(void)
{
    puts_n("x87guest: start\n");
    t_series();
    t_convert();
    t_precision();
    t_remainder();
    t_misc();
    hex("cw.end", get_cw());
    puts_n("x87guest: done\n");
    exit(7);
}
