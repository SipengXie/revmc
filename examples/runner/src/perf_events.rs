//! Six-event perf_event_open bracket used to wrap individual JIT calls with
//! hardware-counter reads (cycles, icache/iTLB misses, IDQ, TMA slots...).
//! Split out of `skeleton_vs_perhash_bench.rs`; kept binary-agnostic so other
//! benches can reuse the same event sets.

use perf_event::events::{Cache, CacheOp, CacheResult, Hardware, WhichCache};
use perf_event::{Builder, Counter, Group};
use std::cell::RefCell;

pub const PERF_NEVENTS: usize = 6;

#[derive(Clone, Copy)]
pub enum PerfEventSet {
    Default,
    Frontend,
    Backend,
    TmaL1,
    TmaL2Bw,
    TmaL3Fetch,
}

impl PerfEventSet {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(PerfEventSet::Default),
            "frontend" => Some(PerfEventSet::Frontend),
            "backend" => Some(PerfEventSet::Backend),
            "tma_l1" => Some(PerfEventSet::TmaL1),
            "tma_l2_bw" => Some(PerfEventSet::TmaL2Bw),
            "tma_l3_fetch" => Some(PerfEventSet::TmaL3Fetch),
            _ => None,
        }
    }

    pub fn names(self) -> [&'static str; PERF_NEVENTS] {
        match self {
            PerfEventSet::Default => {
                ["cycles", "instructions", "L1i_misses", "iTLB_misses", "branches", "branch_misses"]
            }
            PerfEventSet::Frontend => [
                "cycles",
                "instructions",
                "icache_data_stalls",
                "icache_tag_stalls",
                "dsb2mite_penalty_cycles",
                "cycle_activity_stalls_total",
            ],
            PerfEventSet::Backend => [
                "cycles",
                "instructions",
                "cycle_activity_stalls_total",
                "cycle_activity_stalls_l1d_miss",
                "idq_dsb_uops",
                "idq_mite_uops",
            ],
            PerfEventSet::TmaL1 => [
                "cycles",
                "instructions",
                "topdown_slots",
                "uops_retired_slots",
                "topdown_bad_spec_slots",
                "topdown_backend_bound_slots",
            ],
            PerfEventSet::TmaL2Bw => [
                "cycles",
                "instructions",
                "idq_dsb_cycles_any",
                "idq_dsb_cycles_ok",
                "idq_mite_cycles_any",
                "idq_mite_cycles_ok",
            ],
            PerfEventSet::TmaL3Fetch => [
                "cycles",
                "instructions",
                "icache_data_stalls",
                "icache_tag_stalls",
                "offcore_cycles_demand_code_rd",
                "l2_rqsts_code_rd_miss",
            ],
        }
    }
}

// Build a Counter for a raw PERF_TYPE_RAW event with the given config.
// Used for Sapphire Rapids stall/uop events that perf-event 0.4.9 does
// not expose via Hardware/Cache enums.
fn build_raw(group: &mut Group, config: u64) -> std::io::Result<Counter> {
    use perf_event_open_sys::bindings;
    let mut b = Builder::new();
    {
        let attrs = b.attrs_mut();
        attrs.type_ = bindings::PERF_TYPE_RAW;
        attrs.config = config;
    }
    b.group(group).build()
}

pub struct PerfBracket {
    group: Group,
    counters: [Counter; PERF_NEVENTS],
}

impl PerfBracket {
    pub fn new(event_set: PerfEventSet) -> std::io::Result<Self> {
        let mut group = Group::new()?;
        let cycles = Builder::new().group(&mut group).kind(Hardware::CPU_CYCLES).build()?;
        let instructions = Builder::new().group(&mut group).kind(Hardware::INSTRUCTIONS).build()?;
        let counters = match event_set {
            PerfEventSet::Default => {
                let l1i_miss = Builder::new()
                    .group(&mut group)
                    .kind(Cache {
                        which: WhichCache::L1I,
                        operation: CacheOp::READ,
                        result: CacheResult::MISS,
                    })
                    .build()?;
                let itlb_miss = Builder::new()
                    .group(&mut group)
                    .kind(Cache {
                        which: WhichCache::ITLB,
                        operation: CacheOp::READ,
                        result: CacheResult::MISS,
                    })
                    .build()?;
                let branches =
                    Builder::new().group(&mut group).kind(Hardware::BRANCH_INSTRUCTIONS).build()?;
                let branch_misses =
                    Builder::new().group(&mut group).kind(Hardware::BRANCH_MISSES).build()?;
                [cycles, instructions, l1i_miss, itlb_miss, branches, branch_misses]
            }
            PerfEventSet::Frontend => {
                // Sapphire Rapids RAW configs from perf -vv:
                let icache_data = build_raw(&mut group, 0x480)?;
                let icache_tag = build_raw(&mut group, 0x483)?;
                let dsb2mite = build_raw(&mut group, 0x261)?;
                let stalls_total = build_raw(&mut group, 0x40004a3)?;
                [cycles, instructions, icache_data, icache_tag, dsb2mite, stalls_total]
            }
            PerfEventSet::Backend => {
                let stalls_total = build_raw(&mut group, 0x40004a3)?;
                let stalls_l1d_miss = build_raw(&mut group, 0xc000ca3)?;
                let dsb_uops = build_raw(&mut group, 0x879)?;
                let mite_uops = build_raw(&mut group, 0x479)?;
                [cycles, instructions, stalls_total, stalls_l1d_miss, dsb_uops, mite_uops]
            }
            PerfEventSet::TmaL1 => {
                // Sapphire Rapids RAW configs from `perf list --details`.
                // These are top-level TMA slots and form an additive partition:
                // slots = retiring + bad_spec + backend_bound + frontend_bound.
                let topdown_slots = build_raw(&mut group, 0x1a4)?;
                let retired_slots = build_raw(&mut group, 0x2c2)?;
                let bad_spec_slots = build_raw(&mut group, 0x4a4)?;
                let backend_bound_slots = build_raw(&mut group, 0x2a4)?;
                [
                    cycles,
                    instructions,
                    topdown_slots,
                    retired_slots,
                    bad_spec_slots,
                    backend_bound_slots,
                ]
            }
            PerfEventSet::TmaL2Bw => {
                // TMA L2 frontend-bandwidth children use these IDQ cycle events:
                // tma_dsb  = (IDQ.DSB_CYCLES_ANY  - IDQ.DSB_CYCLES_OK)  / core_clks / 2
                // tma_mite = (IDQ.MITE_CYCLES_ANY - IDQ.MITE_CYCLES_OK) / core_clks / 2
                let dsb_cycles_any = build_raw(&mut group, 0x1000879)?;
                let dsb_cycles_ok = build_raw(&mut group, 0x6000879)?;
                let mite_cycles_any = build_raw(&mut group, 0x1000479)?;
                let mite_cycles_ok = build_raw(&mut group, 0x6000479)?;
                [
                    cycles,
                    instructions,
                    dsb_cycles_any,
                    dsb_cycles_ok,
                    mite_cycles_any,
                    mite_cycles_ok,
                ]
            }
            PerfEventSet::TmaL3Fetch => {
                // TMA L3/L4 frontend-latency diagnostics around instruction fetch:
                // tma_icache_misses = ICACHE_DATA.STALLS / thread_clks;
                // tma_code_l2_miss = OFFCORE_REQUESTS_OUTSTANDING.CYCLES_WITH_DEMAND_CODE_RD / thread_clks.
                let icache_data = build_raw(&mut group, 0x480)?;
                let icache_tag = build_raw(&mut group, 0x483)?;
                let offcore_code_rd = build_raw(&mut group, 0x1000220)?;
                let l2_code_miss = build_raw(&mut group, 0x2424)?;
                [cycles, instructions, icache_data, icache_tag, offcore_code_rd, l2_code_miss]
            }
        };
        group.enable()?;
        Ok(Self { group, counters })
    }

    pub fn read(&mut self) -> [u64; PERF_NEVENTS] {
        let c = self.group.read().expect("perf read");
        let mut out = [0u64; PERF_NEVENTS];
        for (i, ctr) in self.counters.iter().enumerate() {
            out[i] = c[ctr];
        }
        out
    }
}

thread_local! {
    static PERF_BRACKET: RefCell<Option<PerfBracket>> = const { RefCell::new(None) };
    static PERF_EVENT_SET: RefCell<PerfEventSet> = const { RefCell::new(PerfEventSet::Default) };
}

pub fn perf_init(event_set: PerfEventSet) {
    PERF_EVENT_SET.with(|p| *p.borrow_mut() = event_set);
    PERF_BRACKET.with(|p| {
        *p.borrow_mut() = Some(
            PerfBracket::new(event_set)
                .expect("perf_event_open: paranoid level too high or event unsupported?"),
        )
    });
}

pub fn perf_snapshot() -> Option<[u64; PERF_NEVENTS]> {
    PERF_BRACKET.with(|p| p.borrow_mut().as_mut().map(|b| b.read()))
}

pub fn perf_event_names() -> [&'static str; PERF_NEVENTS] {
    PERF_EVENT_SET.with(|p| p.borrow().names())
}

