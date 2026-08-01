//! Per-frame timing + perf-bracket dispatch handlers shared by the
//! skeleton-vs-per-hash comparison. Split out of
//! `skeleton_vs_perhash_bench.rs`; the parent binary declares
//! `mod bin_common` / `mod perf_events`, referenced here via `crate::`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use revm::handler::{EvmTr, FrameResult, Handler, ItemOrResult};
use revm::primitives::B256;
use revmc_context::{EvmCompilerFn, RawEvmCompilerFn};

use crate::bin_common::{BenchError, BenchEvm};
use crate::perf_events::{perf_snapshot, PERF_NEVENTS};

pub struct TimingPerHashHandler {
    pub functions: Arc<HashMap<B256, RawEvmCompilerFn>>,
    pub target_hashes: Arc<HashSet<B256>>,
    pub timings: HashMap<B256, Duration>,
    pub perf_counts: HashMap<B256, [u64; PERF_NEVENTS + 1]>,
}

impl Handler for TimingPerHashHandler {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = op_revm::OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame_input)?;
        if let ItemOrResult::Result(frame_result) = res {
            return Ok(frame_result);
        }
        loop {
            let call_or_result = self.run_frame(evm)?;
            let result = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => continue,
                    ItemOrResult::Result(result) => result,
                },
                ItemOrResult::Result(result) => result,
            };
            if let Some(result) = evm.frame_return_result(result)? {
                return Ok(result);
            }
        }
    }
}

impl TimingPerHashHandler {
    fn run_frame(
        &mut self,
        evm: &mut BenchEvm,
    ) -> Result<
        ItemOrResult<revm::interpreter::interpreter_action::FrameInit, FrameResult>,
        BenchError,
    > {
        let (ctx, frame_stack) = (&mut evm.0.ctx, &mut evm.0.frame_stack);
        let frame = frame_stack.get();

        if !crate::bin_common::should_lookup_jit(
            frame.data.is_create(),
            frame.interpreter.input.bytecode_address,
            frame.interpreter.bytecode.is_empty(),
        ) {
            drop((ctx, frame_stack));
            return Ok(evm.frame_run()?);
        }

        let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
        let is_target = self.target_hashes.contains(&bytecode_hash);

        if let Some(&raw_fn) = self.functions.get(&bytecode_hash) {
            let f = EvmCompilerFn::new(raw_fn);
            // perf read (a syscall) stays OUTSIDE the Instant window so the
            // wall-clock sample is not inflated by the counter read itself.
            let perf_before = if is_target { perf_snapshot() } else { None };
            let t0 = if is_target { Some(Instant::now()) } else { None };
            let action = unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) };
            // `elapsed()` must be read before anything else: a compound
            // assignment evaluates its left operand first, so folding this into
            // `entry(..) += t0.elapsed()` charges the hash probe to the frame.
            let dt = t0.map(|t0| t0.elapsed());
            if let Some(before) = perf_before {
                if let Some(after) = perf_snapshot() {
                    let agg =
                        self.perf_counts.entry(bytecode_hash).or_insert([0u64; PERF_NEVENTS + 1]);
                    for i in 0..PERF_NEVENTS {
                        agg[i] = agg[i].wrapping_add(after[i].wrapping_sub(before[i]));
                    }
                    agg[PERF_NEVENTS] += 1;
                }
            }
            if let Some(dt) = dt {
                *self.timings.entry(bytecode_hash).or_default() += dt;
            }
            let result = frame.process_next_action::<_, BenchError>(ctx, action).inspect(|i| {
                if i.is_result() {
                    frame.set_finished(true);
                }
            })?;
            Ok(result)
        } else {
            drop((ctx, frame_stack));
            Ok(evm.frame_run()?)
        }
    }
}

/// Pure-interpreter arm: every frame runs on the native interpreter; target
/// frames get the same exclusive per-segment Instant window as the JIT
/// handlers (each resume segment of a frame is timed separately and summed).
pub struct TimingInterpreterHandler {
    pub target_hashes: Arc<HashSet<B256>>,
    pub timings: HashMap<B256, Duration>,
    pub perf_counts: HashMap<B256, [u64; PERF_NEVENTS + 1]>,
}

impl Handler for TimingInterpreterHandler {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = op_revm::OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame_input)?;
        if let ItemOrResult::Result(frame_result) = res {
            return Ok(frame_result);
        }
        loop {
            let call_or_result = self.run_frame(evm)?;
            let result = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => continue,
                    ItemOrResult::Result(result) => result,
                },
                ItemOrResult::Result(result) => result,
            };
            if let Some(result) = evm.frame_return_result(result)? {
                return Ok(result);
            }
        }
    }
}

impl TimingInterpreterHandler {
    fn run_frame(
        &mut self,
        evm: &mut BenchEvm,
    ) -> Result<
        ItemOrResult<revm::interpreter::interpreter_action::FrameInit, FrameResult>,
        BenchError,
    > {
        let is_target = {
            let frame_stack = &mut evm.0.frame_stack;
            let frame = frame_stack.get();
            if !crate::bin_common::should_lookup_jit(
                frame.data.is_create(),
                frame.interpreter.input.bytecode_address,
                frame.interpreter.bytecode.is_empty(),
            ) {
                false
            } else {
                self.target_hashes.contains(&frame.interpreter.bytecode.get_or_calculate_hash())
            }
        };

        if !is_target {
            return Ok(evm.frame_run()?);
        }

        let bytecode_hash = evm.0.frame_stack.get().interpreter.bytecode.get_or_calculate_hash();
        let perf_before = perf_snapshot();
        let t0 = Instant::now();
        let out = evm.frame_run()?;
        let dt = t0.elapsed();
        if let Some(before) = perf_before {
            if let Some(after) = perf_snapshot() {
                let agg = self.perf_counts.entry(bytecode_hash).or_insert([0u64; PERF_NEVENTS + 1]);
                for i in 0..PERF_NEVENTS {
                    agg[i] = agg[i].wrapping_add(after[i].wrapping_sub(before[i]));
                }
                agg[PERF_NEVENTS] += 1;
            }
        }
        *self.timings.entry(bytecode_hash).or_default() += dt;
        Ok(out)
    }
}

pub struct TimingSkeletonHandler {
    pub per_hash_fns: Arc<HashMap<B256, RawEvmCompilerFn>>,
    pub skeleton_dispatch: Arc<HashMap<B256, (EvmCompilerFn, Vec<u8>)>>,
    pub target_hashes: Arc<HashSet<B256>>,
    pub timings: HashMap<B256, Duration>,
    pub perf_counts: HashMap<B256, [u64; PERF_NEVENTS + 1]>,
}

impl Handler for TimingSkeletonHandler {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = op_revm::OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame_input)?;
        if let ItemOrResult::Result(frame_result) = res {
            return Ok(frame_result);
        }
        loop {
            let call_or_result = self.run_frame(evm)?;
            let result = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => continue,
                    ItemOrResult::Result(result) => result,
                },
                ItemOrResult::Result(result) => result,
            };
            if let Some(result) = evm.frame_return_result(result)? {
                return Ok(result);
            }
        }
    }
}

impl TimingSkeletonHandler {
    fn run_frame(
        &mut self,
        evm: &mut BenchEvm,
    ) -> Result<
        ItemOrResult<revm::interpreter::interpreter_action::FrameInit, FrameResult>,
        BenchError,
    > {
        let (ctx, frame_stack) = (&mut evm.0.ctx, &mut evm.0.frame_stack);
        let frame = frame_stack.get();

        if !crate::bin_common::should_lookup_jit(
            frame.data.is_create(),
            frame.interpreter.input.bytecode_address,
            frame.interpreter.bytecode.is_empty(),
        ) {
            drop((ctx, frame_stack));
            return Ok(evm.frame_run()?);
        }

        let bytecode_hash = frame.interpreter.bytecode.get_or_calculate_hash();
        let is_target = self.target_hashes.contains(&bytecode_hash);

        if let Some((skel_fn, data_table)) = self.skeleton_dispatch.get(&bytecode_hash) {
            let perf_before = if is_target { perf_snapshot() } else { None };
            let t0 = if is_target { Some(Instant::now()) } else { None };
            let action = unsafe {
                skel_fn.call_with_interpreter_data(&mut frame.interpreter, ctx, data_table.as_ptr())
            };
            let dt = t0.map(|t0| t0.elapsed());
            if let Some(before) = perf_before {
                if let Some(after) = perf_snapshot() {
                    let agg =
                        self.perf_counts.entry(bytecode_hash).or_insert([0u64; PERF_NEVENTS + 1]);
                    for i in 0..PERF_NEVENTS {
                        agg[i] = agg[i].wrapping_add(after[i].wrapping_sub(before[i]));
                    }
                    agg[PERF_NEVENTS] += 1;
                }
            }
            if let Some(dt) = dt {
                *self.timings.entry(bytecode_hash).or_default() += dt;
            }
            let result = frame.process_next_action::<_, BenchError>(ctx, action).inspect(|i| {
                if i.is_result() {
                    frame.set_finished(true);
                }
            })?;
            Ok(result)
        } else if let Some(&raw_fn) = self.per_hash_fns.get(&bytecode_hash) {
            let f = EvmCompilerFn::new(raw_fn);
            let action = unsafe { f.call_with_interpreter(&mut frame.interpreter, ctx) };
            let result = frame.process_next_action::<_, BenchError>(ctx, action).inspect(|i| {
                if i.is_result() {
                    frame.set_finished(true);
                }
            })?;
            Ok(result)
        } else {
            drop((ctx, frame_stack));
            Ok(evm.frame_run()?)
        }
    }
}

