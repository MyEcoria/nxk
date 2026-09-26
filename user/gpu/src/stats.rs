//! The stats page (`STATS`): state, clock settings and busy time,
//! mapped read-only by clients, who read it without asking the service
//! (busy for seconds under a benchmark). Busy time is to the GPU what
//! a core's busy time is to `sysinfo`: batches from doorbell to fence.

use libnx::abi::proto::gpu as proto;
use libnx::seqpage::SeqPage;
use libnx::time;

pub struct Stats {
    page: SeqPage,
    pub instance: u64,
    busy_ns: u64,
    since: u64,
}

impl Stats {
    pub fn new() -> Result<Self, i64> {
        let s = Self {
            page: SeqPage::new()?,
            instance: time::now_ns(),
            busy_ns: 0,
            since: 0,
        };
        s.write(&[(proto::S_INSTANCE, s.instance)]);
        Ok(s)
    }

    pub fn write(&self, words: &[(usize, u64)]) {
        self.page.write(words);
    }

    /// A batch rang its doorbell at `t`.
    pub fn start(&mut self, t: u64) {
        self.since = t;
        self.write(&[(proto::S_SINCE_NS, t)]);
    }

    /// The batch in flight ended at `t` (done, failed or timed out).
    pub fn end(&mut self, t: u64) {
        self.busy_ns += t.saturating_sub(self.since);
        self.since = 0;
        self.write(&[(proto::S_BUSY_NS, self.busy_ns), (proto::S_SINCE_NS, 0)]);
    }

    /// A handle for a client: read-only.
    pub fn handle(&self) -> Result<u32, i64> {
        self.page.share()
    }
}
