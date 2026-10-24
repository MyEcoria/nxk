//! The CPU clock while a stream runs: the decoder wants every cycle the
//! chip's rail allows. The highest step the `cpufreq` service accepts is
//! set at the start of a stream and the step that was set comes back at its
//! end (the service refuses a step the rail cannot give, and returns to the
//! boot clock by itself when the SoC gets hot).

use nxgfx::os::Service;
use nxk_abi::proto::cpufreq;
use nxk_abi::user::Msg;

pub struct CpuBoost {
    svc: Service,
    /// The step set before: MHz, or `cpufreq::BOOT`.
    before: u64,
}

impl CpuBoost {
    /// Raises the clock to the highest step the chip runs; `None` when
    /// there is no `cpufreq` service or no higher step.
    pub fn raise() -> Option<Self> {
        let svc = Service::lookup("cpufreq").ok()?;
        let st = svc.call(&Msg::new(cpufreq::STATUS, 0, 0)).ok()?;
        let (steps, before) = (st.mr[4], st.mr[5]);
        let best = (0..cpufreq::STEPS_MHZ.len())
            .rev()
            .find(|i| (steps >> i) & 1 == 1)
            .map(|i| cpufreq::STEPS_MHZ[i])?;
        if best == before {
            return None;
        }
        // The steps are tried from the top: a refused one (ERANGE) leaves the clock alone.
        for mhz in cpufreq::STEPS_MHZ.iter().rev().filter(|&&m| m <= best) {
            if svc.call(&Msg::from_words(cpufreq::SET, &[*mhz])).is_ok() {
                println!("luna: CPU at {} MHz for the stream", mhz);
                return Some(Self { svc, before });
            }
        }
        None
    }
}

impl Drop for CpuBoost {
    fn drop(&mut self) {
        let _ = self
            .svc
            .call(&Msg::from_words(cpufreq::SET, &[self.before]));
    }
}
