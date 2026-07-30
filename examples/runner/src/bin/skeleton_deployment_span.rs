//! Measure actual on-chain deployment time span for the top skeleton groups.
//!
//! Rationale: `push_variance` shows invariants are 100% stable within the 10K-block
//! window. This tool extends the claim by finding, via archive-RPC binary search,
//! the deployment block of each skeleton-group member. If the top group's members
//! span many months of mainnet history, the invariant stability claim is no longer
//! tied to a 5-hour measurement window.

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use revm::primitives::{Address, B256};
use revmc::emse::SkeletonMode;
use serde::Serialize;

use crate::skelot_common::{
    extract_emse_skeleton, extract_opcode_skeleton, hash_bytes, iter_bytecode_raw, load_state_v1,
    should_skip_bytes, ChainId,
};

static RPC_URL: OnceLock<String> = OnceLock::new();
static RATE: OnceLock<RateLimiter> = OnceLock::new();

#[derive(Parser)]
#[command(name = "skeleton_deployment_span")]
struct Args {
    #[arg(long, default_value = "base")]
    chain: String,
    #[arg(long)]
    rpc_url: Option<String>,
    #[arg(long, default_value = "ALCHEMY_API_KEY")]
    api_key_env: String,
    #[arg(long)]
    bench_dir: Option<String>,
    #[arg(long)]
    start_block: Option<u64>,
    #[arg(long)]
    block_count: Option<u64>,
    #[arg(long, default_value_t = 5)]
    top: usize,
    #[arg(long, default_value_t = 30)]
    sample_per_group: usize,
    #[arg(long, default_value_t = 12)]
    concurrency: usize,
    #[arg(long, default_value_t = 12.0)]
    rps: f64,
    #[arg(long, default_value_t = 42u64)]
    seed: u64,
    #[arg(long, default_value = "current")]
    skeleton_mode: SkeletonMode,
    #[arg(long)]
    out: Option<String>,
}

fn resolve_rpc_url(chain: ChainId, override_url: Option<&str>, api_key_env: &str) -> String {
    if let Some(u) = override_url {
        return u.to_string();
    }
    let key = std::env::var(api_key_env).unwrap_or_default();
    match chain {
        ChainId::Base => format!("https://base-mainnet.g.alchemy.com/v2/{key}"),
        ChainId::Eth => format!("https://eth-mainnet.g.alchemy.com/v2/{key}"),
        ChainId::Bsc if key.is_empty() => "https://bsc-dataseed.binance.org/".to_string(),
        ChainId::Bsc => format!("https://bnb-mainnet.g.alchemy.com/v2/{key}"),
        ChainId::Arb => format!("https://arb-mainnet.g.alchemy.com/v2/{key}"),
    }
}

fn classify_rpc(chain: ChainId, has_override: bool, url: &str) -> &'static str {
    if has_override {
        return "override";
    }
    match chain {
        ChainId::Base => "alchemy_base",
        ChainId::Eth => "alchemy_eth",
        ChainId::Arb => "alchemy_arb",
        ChainId::Bsc => {
            if url.contains("bsc-dataseed") {
                "bsc_dataseed"
            } else {
                "alchemy_bsc"
            }
        }
    }
}

fn mask_key(url: &str) -> String {
    if let Some(idx) = url.rfind('/') {
        let (prefix, key) = url.split_at(idx + 1);
        if key.len() > 8 {
            return format!("{prefix}{}\u{2026}{}", &key[..4], &key[key.len() - 4..]);
        }
    }
    url.to_string()
}

/// Replace any whitespace-delimited token that looks like the configured RPC URL
/// with the masked form, so retry messages from `ureq::Error` (which embed the
/// full URL with the API key in their Display impl) do not leak the key.
fn mask_url_in_message(msg: &str) -> String {
    let Some(full_url) = RPC_URL.get() else { return msg.to_string() };
    if full_url.is_empty() {
        return msg.to_string();
    }
    msg.replace(full_url.as_str(), &mask_key(full_url))
}

struct RateLimiter {
    interval: Duration,
    last: Mutex<Instant>,
}

impl RateLimiter {
    fn new(rps: f64) -> Self {
        Self {
            interval: Duration::from_secs_f64(1.0 / rps),
            last: Mutex::new(Instant::now() - Duration::from_secs(1)),
        }
    }
    fn acquire(&self) {
        let mut g = self.last.lock().unwrap();
        let now = Instant::now();
        let earliest = *g + self.interval;
        if now < earliest {
            std::thread::sleep(earliest - now);
        }
        *g = Instant::now();
    }
}

fn hash_skeleton_for_mode(bytes: &[u8], mode: SkeletonMode) -> u64 {
    match mode {
        SkeletonMode::Current => hash_bytes(&extract_opcode_skeleton(bytes)),
        SkeletonMode::Emse => hash_bytes(&extract_emse_skeleton(bytes)),
    }
}

fn rpc_call(method: &str, params: serde_json::Value) -> serde_json::Value {
    let url = RPC_URL.get().expect("RPC_URL not initialized").as_str();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let mut attempt = 0;
    loop {
        // Inside the loop so retries are rate-limited too.
        RATE.get().expect("RATE not initialized").acquire();
        match ureq::post(url).set("Content-Type", "application/json").send_json(&body) {
            Ok(resp) => match resp.into_json::<serde_json::Value>() {
                Ok(v) => return v,
                Err(e) => {
                    eprintln!("json parse error: {e}");
                }
            },
            Err(e) => {
                // ureq formats the full URL (with API key) into its Display impl;
                // mask it before printing so retries do not leak the key.
                let raw = e.to_string();
                let masked = mask_url_in_message(&raw);
                eprintln!("rpc error ({method}): {masked}");
            }
        }
        attempt += 1;
        if attempt > 5 {
            panic!("rpc {method} failed after 5 retries");
        }
        thread::sleep(Duration::from_millis(500 * attempt));
    }
}

fn eth_block_number() -> u64 {
    let v = rpc_call("eth_blockNumber", serde_json::json!([]));
    let hex = v["result"].as_str().unwrap().trim_start_matches("0x");
    u64::from_str_radix(hex, 16).unwrap()
}

fn eth_get_code_len(address: &Address, block: u64) -> usize {
    let v = rpc_call(
        "eth_getCode",
        serde_json::json!([
            format!("0x{}", hex::encode(address.as_slice())),
            format!("0x{:x}", block)
        ]),
    );
    // A JSON-RPC error response (e.g. pruned state on a non-archive node) has
    // no `result`; treating it as code length 0 would silently corrupt the
    // deployment-block binary search. Fail loudly instead.
    if let Some(err) = v.get("error") {
        panic!("eth_getCode error at block {block} for 0x{}: {err}", hex::encode(address));
    }
    let hex = v["result"].as_str().unwrap_or("0x").trim_start_matches("0x");
    hex.len() / 2
}

fn eth_block_timestamp(block: u64) -> u64 {
    let v = rpc_call("eth_getBlockByNumber", serde_json::json!([format!("0x{:x}", block), false]));
    let ts_hex = v["result"]["timestamp"].as_str().unwrap().trim_start_matches("0x");
    u64::from_str_radix(ts_hex, 16).unwrap()
}

/// Binary-search the first block where `address` has non-empty code.
/// Returns None if it has code at block 1 already (shouldn't happen for user contracts)
/// or if it never has code (observation mismatch).
fn find_deployment_block(address: &Address, head: u64) -> Option<u64> {
    if eth_get_code_len(address, head) == 0 {
        return None;
    }
    let mut lo = 1u64;
    let mut hi = head;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if eth_get_code_len(address, mid) == 0 {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Some(lo)
}

/// Probe the configured RPC for archive-state availability before starting the
/// family loop. We pick a sentinel contract deployed long before `block` and
/// require `eth_getCode(addr, block)` to return non-empty code; a non-archive
/// endpoint will return `0x` for any pre-recent block, making the deployment
/// binary search meaningless.
fn probe_archive_rpc(chain: ChainId) {
    let (addr, block) = match chain {
        ChainId::Base => ("0x4200000000000000000000000000000000000006", 1_000_000u64),
        ChainId::Eth => ("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 14_000_000),
        ChainId::Bsc => ("0x10ED43C718714eb63d5aA57B78B54704E256024E", 8_000_000),
        ChainId::Arb => ("0x1F98431c8aD98523631AE4a59f267346ea31F984", 100_000),
    };
    let v = rpc_call("eth_getCode", serde_json::json!([addr, format!("0x{block:x}")]));
    let code: String = serde_json::from_value(v["result"].clone()).unwrap_or_default();
    if code.len() < 4 || code == "0x" {
        panic!(
            "archive RPC probe FAILED for chain {}: eth_getCode({addr},{block}) returned empty.\n\
             This endpoint is not an archive node — binary search for first deployment block will be unreliable.\n\
             Use a different --rpc-url that exposes historical state.",
            chain.as_lowercase()
        );
    }
    eprintln!(
        "[deployment_span] archive probe OK: {addr}@{block} returned {} bytes",
        code.len() / 2
    );
}

#[derive(Serialize)]
struct DeploymentRecord {
    address: String,
    first_block: u64,
    ts_unix: u64,
}

#[derive(Serialize)]
struct FamilySpan {
    rank: usize,
    skeleton_hash: String,
    family_size_in_corpus: usize,
    members_attempted: usize,
    members_resolved: usize,
    deployments: Vec<DeploymentRecord>,
    span_days_min: f64,
    span_days_p5: f64,
    span_days_median: f64,
    span_days_p95: f64,
    span_days_max: f64,
}

#[derive(Serialize)]
struct DeploymentSpansOut {
    chain: String,
    rpc_url_kind: String,
    skeleton_mode: String,
    seed: u64,
    top_n: usize,
    sample_per_group: usize,
    families: Vec<FamilySpan>,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn main() {
    let args = Args::parse();
    let chain: ChainId = args.chain.parse().expect("invalid --chain");
    let has_override = args.rpc_url.is_some();
    let url = resolve_rpc_url(chain, args.rpc_url.as_deref(), &args.api_key_env);
    eprintln!("[deployment_span] chain={} rpc_url={}", chain.as_lowercase(), mask_key(&url));
    let rpc_kind = classify_rpc(chain, has_override, &url).to_string();
    if RPC_URL.set(url).is_err() {
        panic!("RPC_URL already set");
    }
    if RATE.set(RateLimiter::new(args.rps)).is_err() {
        panic!("RATE already set");
    }
    probe_archive_rpc(chain);

    let (default_dir, default_start, default_count) =
        crate::skelot_common::chain_data_defaults(chain);
    let dir = args.bench_dir.unwrap_or_else(|| default_dir.to_string());
    let bench_dir = Path::new(&dir);
    let start = args.start_block.unwrap_or(default_start);
    let count = args.block_count.unwrap_or(default_count);
    let end = start + count;

    eprintln!(
        "[deployment_span] loading codes + account index from blocks {start}..{end} ({count} blocks)"
    );
    let mut all_codes: HashMap<B256, Vec<u8>> = HashMap::new();
    let mut code_hash_to_addr: HashMap<B256, Address> = HashMap::new();
    let mut scanned = 0u64;
    for bn in start..end {
        let path = bench_dir.join(format!("states/{bn}.bin"));
        let snap = match load_state_v1(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        iter_bytecode_raw(&snap, |hash, raw| {
            if should_skip_bytes(&raw) {
                return;
            }
            all_codes.entry(*hash).or_insert(raw);
        });
        for (addr, acct) in &snap.accounts {
            if let Some(ref info) = acct.info {
                code_hash_to_addr.entry(info.code_hash).or_insert(*addr);
            }
        }
        scanned += 1;
        if scanned % 2000 == 0 {
            eprintln!(
                "  {scanned} blocks, {} codes, {} addr-mappings",
                all_codes.len(),
                code_hash_to_addr.len()
            );
        }
    }
    eprintln!(
        "[deployment_span] scanned {scanned} blocks: {} unique bytecodes, {} code_hash->addr mappings",
        all_codes.len(),
        code_hash_to_addr.len()
    );
    eprintln!("[deployment_span] skeleton_mode={}", args.skeleton_mode.as_str());

    let mut skeleton_groups: HashMap<u64, Vec<B256>> = HashMap::new();
    for (hash, bytes) in &all_codes {
        if bytes.is_empty() {
            continue;
        }
        skeleton_groups
            .entry(hash_skeleton_for_mode(bytes, args.skeleton_mode))
            .or_default()
            .push(*hash);
    }
    let mut dup_groups: Vec<(u64, Vec<B256>)> =
        skeleton_groups.into_iter().filter(|(_, m)| m.len() >= 2).collect();
    // Sort each family's member list so the first-N take in the sampling step is
    // reproducible across processes (HashMap iteration above is RandomState-keyed).
    for (_, members) in dup_groups.iter_mut() {
        members.sort();
    }
    dup_groups.sort_by_key(|(_, m)| std::cmp::Reverse(m.len()));

    eprintln!("[deployment_span] querying head block...");
    let head = eth_block_number();
    eprintln!("[deployment_span] head block: {head}");

    let mut families: Vec<FamilySpan> = Vec::new();
    for rank in 0..args.top.min(dup_groups.len()) {
        let (skel_hash, members) = &dup_groups[rank];
        let n = members.len();
        let bytecode_len = all_codes[&members[0]].len();

        // Pick members that have addresses in our cache. The current sampler is
        // a deterministic first-N take rather than a seeded shuffle: the runner
        // crate has no `rand` dependency, so we record the configured seed in
        // the JSON metadata for reproducibility but do not actually consume it.
        let eligible: Vec<(B256, Address)> =
            members.iter().filter_map(|h| code_hash_to_addr.get(h).map(|a| (*h, *a))).collect();
        let take = args.sample_per_group.min(eligible.len());
        let sampled: Vec<(B256, Address)> = eligible.into_iter().take(take).collect();
        let attempted = sampled.len();

        eprintln!(
            "\n[deployment_span] group #{} (skel {:#x}): {} members, bytecode {} bytes -- sampling {} with addresses",
            rank + 1,
            skel_hash,
            n,
            bytecode_len,
            attempted
        );

        // Query deployment blocks in parallel.
        let deploy_blocks: Arc<Mutex<Vec<(Address, Option<u64>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let counter = Arc::new(Mutex::new(0usize));
        let total = sampled.len();
        let chunks: Vec<Vec<(B256, Address)>> = {
            let mut v: Vec<Vec<(B256, Address)>> =
                (0..args.concurrency).map(|_| Vec::new()).collect();
            for (i, item) in sampled.into_iter().enumerate() {
                v[i % args.concurrency].push(item);
            }
            v
        };

        thread::scope(|s| {
            for chunk in chunks {
                let deploy_blocks = Arc::clone(&deploy_blocks);
                let counter = Arc::clone(&counter);
                s.spawn(move || {
                    for (_hash, addr) in chunk {
                        let db = find_deployment_block(&addr, head);
                        {
                            let mut guard = deploy_blocks.lock().unwrap();
                            guard.push((addr, db));
                        }
                        let mut c = counter.lock().unwrap();
                        *c += 1;
                        if *c % 10 == 0 {
                            eprintln!("  {}/{}", *c, total);
                        }
                    }
                });
            }
        });

        // Collect resolved (addr, deploy_block) pairs.
        let resolved: Vec<(Address, u64)> = {
            let guard = deploy_blocks.lock().unwrap();
            guard.iter().filter_map(|(a, db)| db.map(|b| (*a, b))).collect()
        };

        if resolved.is_empty() {
            eprintln!("[deployment_span] group #{}: no deployment blocks resolved", rank + 1);
            families.push(FamilySpan {
                rank: rank + 1,
                skeleton_hash: format!("{skel_hash:#x}"),
                family_size_in_corpus: n,
                members_attempted: attempted,
                members_resolved: 0,
                deployments: Vec::new(),
                span_days_min: f64::NAN,
                span_days_p5: f64::NAN,
                span_days_median: f64::NAN,
                span_days_p95: f64::NAN,
                span_days_max: f64::NAN,
            });
            continue;
        }

        // Resolve timestamps for every resolved member.
        let mut deployments: Vec<DeploymentRecord> = resolved
            .iter()
            .map(|(addr, db)| {
                let ts = eth_block_timestamp(*db);
                DeploymentRecord {
                    address: format!("0x{}", hex::encode(addr.as_slice())),
                    first_block: *db,
                    ts_unix: ts,
                }
            })
            .collect();
        deployments.sort_by_key(|d| d.first_block);

        let min_ts = deployments.first().unwrap().ts_unix;
        let max_ts = deployments.last().unwrap().ts_unix;
        let mut spans: Vec<f64> = deployments
            .iter()
            .map(|d| (d.ts_unix.saturating_sub(min_ts)) as f64 / 86_400.0)
            .collect();
        spans.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let span_days_min = spans.first().copied().unwrap_or(f64::NAN);
        let span_days_max = spans.last().copied().unwrap_or(f64::NAN);
        let span_days_median = percentile(&spans, 0.5);
        let span_days_p5 = percentile(&spans, 0.05);
        let span_days_p95 = percentile(&spans, 0.95);

        eprintln!(
            "[deployment_span] group #{}: {} resolved / {} attempted; span min={:.1}d median={:.1}d max={:.1}d ({} -> {})",
            rank + 1,
            deployments.len(),
            attempted,
            span_days_min,
            span_days_median,
            span_days_max,
            fmt_ts(min_ts),
            fmt_ts(max_ts)
        );

        families.push(FamilySpan {
            rank: rank + 1,
            skeleton_hash: format!("{skel_hash:#x}"),
            family_size_in_corpus: n,
            members_attempted: attempted,
            members_resolved: deployments.len(),
            deployments,
            span_days_min,
            span_days_p5,
            span_days_median,
            span_days_p95,
            span_days_max,
        });
    }

    let out = DeploymentSpansOut {
        chain: chain.as_lowercase().to_string(),
        rpc_url_kind: rpc_kind,
        skeleton_mode: args.skeleton_mode.as_str().to_string(),
        seed: args.seed,
        top_n: args.top,
        sample_per_group: args.sample_per_group,
        families,
    };

    if let Some(out_path) = args.out.as_deref() {
        let json = serde_json::to_string_pretty(&out).expect("serialize spans");
        std::fs::write(out_path, json).unwrap_or_else(|e| panic!("write {out_path}: {e}"));
        eprintln!("[deployment_span] wrote {out_path}");
    } else {
        let json = serde_json::to_string_pretty(&out).expect("serialize spans");
        println!("{json}");
    }
}

fn fmt_ts(ts: u64) -> String {
    // Seconds since epoch -> YYYY-MM-DD HH:MM:SS UTC, minimal impl.
    let days = (ts / 86400) as i64;
    let (year, month, day) = days_to_ymd(days);
    let rem = ts % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", year, month, day, h, m, s)
}

fn days_to_ymd(days_since_epoch: i64) -> (i32, u32, u32) {
    // Based on Howard Hinnant's date algorithms.
    let z = days_since_epoch + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m as u32, d as u32)
}
