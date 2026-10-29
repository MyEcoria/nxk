/* Native reference run on the PC: the imports the runtime provides as HLE, as Linux syscalls. */
typedef unsigned long size_t;

long write(int fd, const void *buf, size_t len)
{
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(1), "D"(fd), "S"(buf), "d"(len) : "rcx", "r11", "memory");
    return r;
}

void _exit(int code)
{
    __asm__ volatile("syscall" : : "a"(231), "D"(code) : "rcx", "r11", "memory");
    __builtin_unreachable();
}

/* The kernel enters with rsp 16-aligned; a call gives _start the alignment of a function entry. */
__asm__(".globl native_entry\nnative_entry:\n call _start\n hlt\n");
