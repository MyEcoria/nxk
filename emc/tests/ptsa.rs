//! The PTSA rates NVIDIA's bpmpfw.bin wrote in hekate's emulated run (DRAM
//! table at bl31's entry), and two rates hekate trims, whose MLL and third
//! words equal the shipped tables'.
use nxk_emc::bpmp_ptsa_rates;

#[test]
fn bpmp_ptsa_rates_match_bpmpfw() {
    let seen = [
        (204_000, [0x1b, 0x3, 0x24c]),
        (408_000, [0x35, 0x6, 0x498]),
        (800_000, [0x68, 0xc, 0x902]),
        (1_331_200, [0xad, 0x14, 0xefc]),
        (1_600_000, [0xd0, 0x18, 0x1203]),
        (665_600, [0x57, 0xa, 0x77e]),
        (1_065_600, [0x8b, 0x10, 0xbff]),
    ];
    for (khz, want) in seen {
        assert_eq!(bpmp_ptsa_rates(khz), want, "{} kHz", khz);
    }
}
