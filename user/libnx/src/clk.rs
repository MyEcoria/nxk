//! Clock and reset client (`clk` service, `proto::clk`).

use nxk_abi::proto::clk;

use crate::ipc::Msg;
use crate::ns::Service;

static CLK: Service = Service::new("clk", 2_000_000_000);

fn request(label: u32, dev: u64) -> Result<Msg, i64> {
    CLK.call(&Msg::from_words(label, &[dev]))
}

/// Clock `dev` on and out of reset (`clk::*` device).
pub fn enable(dev: u64) -> Result<(), i64> {
    request(clk::ENABLE, dev).map(|_| ())
}

pub fn disable(dev: u64) -> Result<(), i64> {
    request(clk::DISABLE, dev).map(|_| ())
}

/// State bits (`clk::ST_*`) of `dev`, and its extra word.
pub fn status(dev: u64) -> Result<(u64, u64), i64> {
    request(clk::STATUS, dev).map(|r| (r.mr[0], r.mr[1]))
}

/// `dev` fully powered, clocked and out of reset: its registers are
/// safe to touch.
pub fn ready(dev: u64) -> bool {
    matches!(status(dev), Ok((st, _)) if st & clk::ST_READY == clk::ST_READY)
}
