//! Minerva's training and periodic compensation: what `change_clock`
//! calls while training or for a periodic-training entry.

use crate::{Hw, Table, TableMut};

/// Minerva's `comp_seq_t`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Comp {
    Dvfs = 1,
    WriteTraining = 2,
    PeriodicTraining = 3,
}

pub(crate) fn periodic_compensation_handler<H: Hw>(
    _h: &mut H,
    _src: &Table,
    _dst: &mut TableMut,
    _dev_num: u32,
    _ch1: bool,
    _seq: Comp,
) -> u32 {
    unimplemented!("periodic compensation")
}

pub(crate) fn apply_periodic_compensation_trimmer(_dst: &TableMut, _addr: u32) -> u32 {
    unimplemented!("compensation trimmer")
}

pub(crate) fn save_train_results<H: Hw>(_h: &mut H, _dst: &mut TableMut, _needs_training: u32, _dev_num: u32, _ch1: bool) {
    unimplemented!("save training results")
}
