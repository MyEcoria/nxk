// Test oracle: register macros of Minerva's mtc.h redirected to a recording
// bus (oracle.cpp). Spliced into a scratch copy of hekate's module by
// build.sh; never part of a firmware build.
#ifndef ORACLE_REGS_H
#define ORACLE_REGS_H

enum { B_CAR = 0, B_MC = 1, B_EMC = 2, B_TMR = 3, B_MISC = 4, B_FUSE = 5 };

unsigned bus_rd(int bank, unsigned off);
void bus_wr(int bank, unsigned off, unsigned v);

struct Reg {
	int bank;
	unsigned off;
	operator unsigned() const { return bus_rd(bank, off); }
	Reg &operator=(unsigned v) { bus_wr(bank, off, v); return *this; }
	Reg &operator|=(unsigned v) { bus_wr(bank, off, bus_rd(bank, off) | v); return *this; }
	Reg &operator&=(unsigned v) { bus_wr(bank, off, bus_rd(bank, off) & v); return *this; }
};

#define TMR(off)      Reg{B_TMR, (unsigned)(off)}
#define CLOCK(off)    Reg{B_CAR, (unsigned)(off)}
#define APB_MISC(off) Reg{B_MISC, (unsigned)(off)}
#define FUSE(off)     Reg{B_FUSE, (unsigned)(off)}
#define MC(off)       Reg{B_MC, (unsigned)(off)}
#define EMC(off)      Reg{B_EMC, (unsigned)(off)}
#define EMC_CH0(off)  Reg{B_EMC, (unsigned)(off) + 0x3000}
#define EMC_CH1(off)  Reg{B_EMC, (unsigned)(off) + 0x4000}

// Constants that sit inside the replaced block of mtc.h.
#define MTC_INIT_MAGIC 0x3043544D
#define MTC_NEW_MAGIC  0x5243544D
#define MTC_IRB_MAGIC  0x4943544D
#define MTC_INIT_FREQUENCY 204000

#endif
