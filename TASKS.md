# Tasks

Planned work, not started. Each entry says what is known; per
`AGENTS.md` section 7, propose the options before coding any of them.

## 1. `sd`: fall back to a 1-bit bus like hekate

With the Erista's microSD connector loose (hardware run 19), only DAT0
reached the card: hekate warned "SD Card Issues Warning" and ran two
different cards in 1-bit mode (initial bus HS25 1bit, 3 init fails, then
no read/write errors). `sd` always starts on a 4-bit bus, so its first
read got no data (DAT lines idle high, card in state 4, ADMA stopped
after the first descriptor fetch). Reseating the connector fixed the
Switch (run 20); the fallback is for robustness on a worn slot.

`recover()` in `user/sd/src/main.rs` retries at 1.8 V, after a 2 s
power cycle, and last on a 1-bit bus after a 239 ms power cycle: all
three failed with EIO and the card stopped answering CMD13, although
hekate succeeded in 1-bit. Why the power-cycled re-inits fail is not
known.

Options discussed: after the first data timeout, switch the card and
host to 1-bit in place (ACMD6 with width 0, HOST_CONTROL bit 1 clear,
read again, no power cycle), keeping the other retries behind it with
1.8 V last; or reorder the retries (3.3 V 1-bit right after the
failure). At 25 MHz a 1-bit bus gives about 3 MB/s.
