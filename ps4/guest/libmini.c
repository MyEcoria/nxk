/* A guest "libc" module: what the game's own libc.prx is to its eboot.
 * Its exports are bound to main's imports by NID; it imports write/_exit from HLE. */
typedef unsigned long size_t;

long write(int fd, const void *buf, size_t len);
void _exit(int code) __attribute__((noreturn));

int mini_errno;

void *memcpy(void *dst, const void *src, size_t n)
{
    unsigned char *d = dst;
    const unsigned char *s = src;
    while (n--)
        *d++ = *s++;
    return dst;
}

void *memset(void *dst, int c, size_t n)
{
    unsigned char *d = dst;
    while (n--)
        *d++ = (unsigned char)c;
    return dst;
}

void *memmove(void *dst, const void *src, size_t n)
{
    unsigned char *d = dst;
    const unsigned char *s = src;
    if (d < s) {
        while (n--)
            *d++ = *s++;
    } else {
        while (n--)
            d[n] = s[n];
    }
    return dst;
}

int memcmp(const void *a, const void *b, size_t n)
{
    const unsigned char *x = a, *y = b;
    for (; n; n--, x++, y++)
        if (*x != *y)
            return *x - *y;
    return 0;
}

size_t strlen(const char *s)
{
    size_t n = 0;
    while (s[n])
        n++;
    return n;
}

void puts_n(const char *s)
{
    if (write(1, s, strlen(s)) < 0)
        mini_errno = 1;
}

/* Decimal (base 10, signed when `sign`) or hexadecimal (base 16) text of v. */
void put_num(long v, int base, int sign)
{
    char buf[24];
    int i = sizeof buf;
    unsigned long u = (sign && v < 0) ? -(unsigned long)v : (unsigned long)v;
    do {
        buf[--i] = "0123456789abcdef"[u % base];
        u /= base;
    } while (u);
    if (sign && v < 0)
        buf[--i] = '-';
    write(1, buf + i, sizeof buf - i);
}

void exit(int code)
{
    _exit(code);
}
