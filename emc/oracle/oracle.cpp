// Test oracle for nxk-emc: runs hekate's Minerva OP_SWITCH on two table
// entries against a modelled register file and prints every access.
//
//   oracle SRC.bin DST.bin REGS.txt > trace.txt
//
// REGS.txt: "seed N" and "<C|M|E> <offset hex> <value hex>" lines (initial
// register values; any other register reads a hash of seed, bank, offset).
// The Rust side (emc/tests/trace.rs) models the same bus; the traces must be
// identical. Trace lines: "R b off val", "W b off val", "U us".
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <string>
#include <unistd.h>

static std::map<unsigned long long, unsigned> mem;
static unsigned seed;

static const char BANKCH[] = {'C', 'M', 'E', 'T', 'A', 'F'};

static unsigned long long key(int bank, unsigned off) { return ((unsigned long long)bank << 32) | off; }

static unsigned mix(unsigned x)
{
	x ^= x >> 16;
	x *= 0x7feb352d;
	x ^= x >> 15;
	x *= 0x846ca68b;
	x ^= x >> 16;
	return x;
}

static unsigned hash(int bank, unsigned off)
{
	return mix(seed ^ mix(bank * 0x9E3779B1u) ^ mix(off * 0x85EBCA6Bu + 1));
}

// Channel-1 copies of the shared status registers read as the base ones.
static unsigned alias(int bank, unsigned off)
{
	if (bank == 2 && off >= 0x4000 && off < 0x5000) {
		unsigned lo = off & 0xFFF;
		if (lo == 0x2BC || lo == 0x2B4 || lo == 0x2C4)
			return lo;
	}
	return off;
}

unsigned bus_rd(int bank, unsigned off)
{
	unsigned a = alias(bank, off);
	auto it = mem.find(key(bank, a));
	unsigned v = it != mem.end() ? it->second : hash(bank, a);
	if (bank == 2 && a == 0x0)
		v |= 0x10; // INTSTATUS: clock change complete
	if (bank == 2 && a == 0x2B4)
		v = 0; // EMC_STATUS: nothing stalled, no read data pending
	if (bank == 2 && a == 0x2C4)
		v |= 0x28000; // DIG_DLL_STATUS: locked
	if (bank == 0 && (a == 0x90 || a == 0x5E8))
		v |= 1u << 27; // PLLM / PLLMB lock
	printf("R %c %x %x\n", BANKCH[bank], off, v);
	return v;
}

void bus_wr(int bank, unsigned off, unsigned v)
{
	mem[key(bank, off)] = v;
	printf("W %c %x %x\n", BANKCH[bank], off, v);
}

void trace_usleep(unsigned us) { printf("U %u\n", us); }

#include "sys_sdrammtc.c"

static void load(const char *path, void *dst, size_t n)
{
	FILE *f = fopen(path, "rb");
	if (!f || fread(dst, 1, n, f) != n) {
		fprintf(stderr, "oracle: cannot read %s\n", path);
		exit(2);
	}
	fclose(f);
}

int main(int argc, char **argv)
{
	if (argc != 4) {
		fprintf(stderr, "usage: oracle SRC.bin DST.bin REGS.txt\n");
		return 2;
	}
	alarm(30);
	static emc_table_t tbl[2];
	load(argv[1], &tbl[0], sizeof(emc_table_t));
	load(argv[2], &tbl[1], sizeof(emc_table_t));

	FILE *f = fopen(argv[3], "r");
	if (!f)
		return 2;
	char line[128];
	while (fgets(line, sizeof line, f)) {
		unsigned off, val, n;
		char b;
		if (sscanf(line, "seed %u", &n) == 1)
			seed = n;
		else if (sscanf(line, "%c %x %x", &b, &off, &val) == 3)
			mem[key(strchr(BANKCH, b) - BANKCH, off)] = val;
	}
	fclose(f);

	mtc_config_t cfg;
	memset(&cfg, 0, sizeof cfg);
	cfg.mtc_table = tbl;
	cfg.table_entries = 2;
	cfg.rate_from = tbl[0].rate_khz;
	cfg.rate_to = tbl[1].rate_khz;
	cfg.train_mode = OP_SWITCH;
	cfg.init_done = MTC_INIT_MAGIC;
	minerva_entry(&cfg, NULL);
	return 0;
}
