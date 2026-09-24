# Tasks

Planned work, not started. Each entry says what is known; per
`AGENTS.md` section 7, propose the options before coding any of them.

## 1. `sd`: fall back to a 1-bit bus like hekate

The Erista's microSD slot carries DAT0 only (hardware run 19): hekate
warns "SD Card Issues Warning" and runs the card in 1-bit mode (initial
bus HS25 1bit, 3 init fails, then no read/write errors), with two
different cards that both work in 4-bit on the PC. `sd` always starts on
a 4-bit bus, so its first read gets no data (DAT lines idle high, card
in state 4, ADMA stopped after the first descriptor fetch).

`recover()` in `user/sd/src/main.rs` then retries at 1.8 V, after a 2 s
power cycle, and last on a 1-bit bus after a 239 ms power cycle: all
three fail with EIO and the card stops answering CMD13, although hekate
succeeds in 1-bit. Why the power-cycled re-inits fail is not known.

Options discussed: after the first data timeout, switch the card and
host to 1-bit in place (ACMD6 with width 0, HOST_CONTROL bit 1 clear,
read again, no power cycle), keeping the other retries behind it with
1.8 V last; or reorder the retries (3.3 V 1-bit right after the
failure). At 25 MHz a 1-bit bus gives about 3 MB/s.

## 2. `sd`: check the direct DMA on the Switch

Since commit `e1b9bb1`, `sd` programs ADMA2 straight on the buffer `fs`
attaches (contiguous, uncached, below 4 GiB), keeping one page for its
descriptors and the first read. Tested in the emulator only: ktest,
stdtest, a 2 MB `--usb-put` then a cold-boot `cp`, both files
byte-identical on the image, 512 KiB less memory. On the Switch it
waits for a card that reads (task 1): run `stdtest` (`files`), a 2 MB
`--put` with `sync`, then after a reboot `cp` it and compare both files
on the PC, and `mem` after the mount.
