/* Main guest program: ordinary C compiled at -O2, its library calls bound by NID.
 * Every check prints a line; the PC's native run of the same code is the reference. */
typedef unsigned long size_t;
typedef unsigned long u64;
typedef unsigned int u32;
typedef unsigned short u16;
typedef unsigned char u8;

void *memcpy(void *, const void *, size_t);
void *memset(void *, int, size_t);
void *memmove(void *, const void *, size_t);
int memcmp(const void *, const void *, size_t);
size_t strlen(const char *);
void puts_n(const char *);
void put_num(long, int, int);
void exit(int) __attribute__((noreturn));
extern int mini_errno;

static void line(const char *name, long v)
{
    puts_n(name);
    puts_n(" ");
    put_num(v, 10, 1);
    puts_n("\n");
}

static void hex(const char *name, u64 v)
{
    puts_n(name);
    puts_n(" 0x");
    put_num((long)v, 16, 0);
    puts_n("\n");
}

static u64 rng = 0x9e3779b97f4a7c15ul;

static u64 xorshift(void)
{
    rng ^= rng << 13;
    rng ^= rng >> 7;
    rng ^= rng << 17;
    return rng;
}

static int cmp_int(const void *a, const void *b)
{
    int x = *(const int *)a, y = *(const int *)b;
    return (x > y) - (x < y);
}

static int cmp_rev(const void *a, const void *b)
{
    return cmp_int(b, a);
}

/* Shell sort through a comparator pointer: indirect calls. */
static void sort(int *v, int n, int (*cmp)(const void *, const void *))
{
    for (int gap = n / 2; gap > 0; gap /= 2)
        for (int i = gap; i < n; i++)
            for (int j = i - gap; j >= 0 && cmp(&v[j], &v[j + gap]) > 0; j -= gap) {
                int t = v[j];
                v[j] = v[j + gap];
                v[j + gap] = t;
            }
}

static void t_sort(void)
{
    static int (*const cmps[2])(const void *, const void *) = {cmp_int, cmp_rev};
    int v[200];
    for (int k = 0; k < 2; k++) {
        for (int i = 0; i < 200; i++)
            v[i] = (int)(xorshift() % 2001) - 1000;
        sort(v, 200, cmps[k]);
        long sum = 0;
        int ok = 1;
        for (int i = 0; i < 200; i++) {
            sum = sum * 31 + v[i];
            if (i && cmps[k](&v[i - 1], &v[i]) > 0)
                ok = 0;
        }
        line(k ? "sort.rev" : "sort.fwd", ok ? sum : -1);
    }
}

static u32 crc_table[256];

static void t_crc(void)
{
    for (u32 i = 0; i < 256; i++) {
        u32 c = i;
        for (int k = 0; k < 8; k++)
            c = c & 1 ? 0xedb88320u ^ (c >> 1) : c >> 1;
        crc_table[i] = c;
    }
    const char *text = "The quick brown fox jumps over the lazy dog";
    u32 c = ~0u;
    for (size_t i = 0; i < strlen(text); i++)
        c = crc_table[(c ^ (u8)text[i]) & 0xff] ^ (c >> 8);
    hex("crc32", ~c);
}

static long fib(int n)
{
    return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

static int ackermann(int m, int n)
{
    if (m == 0)
        return n + 1;
    if (n == 0)
        return ackermann(m - 1, 1);
    return ackermann(m - 1, ackermann(m, n - 1));
}

static int __attribute__((noinline)) dispatch(int op, int a, int b)
{
    switch (op) {
    case 0: return a + b;
    case 1: return a - b;
    case 2: return a * b;
    case 3: return b ? a / b : 0;
    case 4: return b ? a % b : 0;
    case 5: return a << (b & 15);
    case 6: return a >> (b & 15);
    case 7: return (unsigned)a >> (b & 15);
    case 8: return a & b;
    case 9: return a | b;
    case 10: return a ^ b;
    case 11: return a < b;
    default: return -1;
    }
}

static void t_switch(void)
{
    long acc = 0;
    for (int i = 0; i < 1000; i++) {
        int a = (int)xorshift(), b = (int)(xorshift() % 97) - 48;
        acc = acc * 7 + dispatch(i % 13, a, b);
    }
    line("switch", acc);
}

struct big {
    long a[17];
    char tag[13];
    short s;
};

static struct big __attribute__((noinline)) make_big(int seed)
{
    struct big b;
    for (int i = 0; i < 17; i++)
        b.a[i] = seed * (i + 1);
    memcpy(b.tag, "struct-copy!", 13);
    b.s = (short)seed;
    return b;
}

static void t_struct(void)
{
    struct big x = make_big(12345), y;
    y = x;
    y.a[3] += 1;
    memmove(y.a + 1, y.a, 8 * sizeof(long));
    long sum = 0;
    for (int i = 0; i < 17; i++)
        sum += y.a[i] ^ x.a[i];
    line("struct", sum + memcmp(&x.tag, &y.tag, 13) + y.s);
    char buf[64];
    memset(buf, 'z', sizeof buf);
    buf[63] = 0;
    line("memset", (long)strlen(buf) * buf[10]);
}

static void t_narrow(void)
{
    u8 b = 200;
    signed char sc = -100;
    u16 w = 65000;
    short sw = -30000;
    long acc = 0;
    for (int i = 0; i < 300; i++) {
        b += 37;
        sc = (signed char)(sc * 3 + 1);
        w = (u16)(w * 7 + b);
        sw = (short)(sw ^ (sw << 3)) + sc;
        acc = acc * 13 + b + sc + w + sw;
    }
    line("narrow", acc);
}

static void t_muldiv(void)
{
    u64 h = 0;
    long s = 0;
    for (int i = 1; i < 500; i++) {
        u64 x = xorshift(), y = xorshift() | 1;
        unsigned __int128 p = (unsigned __int128)x * y;
        h ^= (u64)(p >> 64) + (u64)p;
        h += x / y + x % (y >> 32 | 1);
        long a = (long)xorshift(), d = (long)(xorshift() % 1000) - 500;
        if (d)
            s += a / d + a % d;
        s += (int)x / ((int)i - 250 | 1);
        s += (long)((__int128)a * d >> 64);
    }
    hex("muldiv.u", h);
    line("muldiv.s", s);
}

static void t_bits(void)
{
    u64 acc = 0;
    for (int i = 0; i < 400; i++) {
        u64 x = xorshift();
        int sh = i & 63;
        acc += (x << sh) | (x >> ((64 - sh) & 63));
        acc ^= (u64)((long)x >> (sh / 2));
        if (x)
            acc += __builtin_ctzll(x) * 3 + __builtin_clzll(x);
        acc += (x >> 7 & 1) ? 5 : 9;
        u32 y = (u32)x;
        acc += (y << (i & 31)) | (y >> ((32 - (i & 31)) & 31));
        acc += __builtin_bswap32(y) + __builtin_bswap64(x);
        acc += (int)(x > acc) + (long)x < 0;
    }
    hex("bits", acc);
}

static void t_strings(void)
{
    char a[40], b[40];
    const char *src = "nxk runs PS4 code on Tegra";
    size_t n = strlen(src);
    memcpy(a, src, n + 1);
    for (size_t i = 0; i < n; i++)
        b[i] = a[n - 1 - i];
    b[n] = 0;
    for (size_t i = 0; i < n; i++)
        if (b[i] >= 'a' && b[i] <= 'z')
            b[i] -= 32;
    puts_n(b);
    puts_n("\n");
    line("strcmp", memcmp(a, b, n) > 0);
}

struct node {
    struct node *next;
    long v;
};

static struct node nodes[64];
static struct node *const first = &nodes[0];
static const char *const names[] = {"zero", "one", "two", "three"};

static void t_pointers(void)
{
    for (int i = 0; i < 63; i++) {
        nodes[i].next = &nodes[(i * 5 + 1) % 64];
        nodes[i].v = i * i;
    }
    nodes[63].next = 0;
    long sum = 0;
    int steps = 0;
    for (struct node *p = first; p && steps < 1000; p = p->next, steps++)
        sum += p->v;
    line("list", sum * 1000 + steps);
    for (int i = 0; i < 4; i++)
        puts_n(names[i]);
    puts_n("\n");
    mini_errno = 7;
    line("errno", mini_errno);
}

void _start(void)
{
    puts_n("guest: hello from x86-64\n");
    t_sort();
    t_crc();
    line("fib", fib(20));
    line("ackermann", ackermann(2, 3));
    t_switch();
    t_struct();
    t_narrow();
    t_muldiv();
    t_bits();
    t_strings();
    t_pointers();
    puts_n("guest: done\n");
    exit(3);
}
