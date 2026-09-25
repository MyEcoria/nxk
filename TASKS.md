# Tasks

Planned work, not started. Each entry says what is known; per
`AGENTS.md` section 7, propose the options before coding any of them.

## 1. `sd`: power-cycled re-inits fail

With the Erista's microSD connector loose (hardware run 19), only DAT0
reached the card: hekate warned "SD Card Issues Warning" and ran two
different cards in 1-bit mode (initial bus HS25 1bit, 3 init fails, then
no read/write errors). `sd` read no data on its 4-bit bus (DAT lines
idle high, card in state 4, ADMA stopped after the first descriptor
fetch). Reseating the connector fixed the Switch (run 20).

`recover()` in `user/sd/src/main.rs` now first drops card and host to a
1-bit bus in place (CMD13, CMD12 if the card is still in data state,
ACMD6 with width 0, HOST_CONTROL bit 1 clear), without a power cycle;
not yet run on hardware. Behind it come the other retries, now 4-bit
after 2 s off, 1-bit after 239 ms off, then 1.8 V. In run 19 (1.8 V
first) all three failed with EIO and the card stopped answering CMD13,
although hekate succeeded in 1-bit after its own power cycles. Why is
not known. One
difference: hekate gates the SDMMC1 clock in CAR when it powers the
slot off (`sdmmc_end`) and re-enables it, resetting the controller, on
the next init; `recover()` keeps the controller clocked throughout.
