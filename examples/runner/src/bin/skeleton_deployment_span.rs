//! Measure actual on-chain deployment time span for the top skeleton groups.
//!
//! Rationale: `push_variance` shows invariants are 100% stable within the 10K-block
//! window. This tool extends the claim by finding, via Alchemy RPC binary search,
//! the deployment block of each skeleton-group member. If the top group's members
//! span many months of mainnet history, the invariant stability claim is no longer
//! tied to a 5-hour measurement window.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use revm::bytecode::Bytecode;
use revm::primitives::{Address, HashMap as RevmHashMap, B256, U256};
use revm::state::AccountInfo;
use serde::Deserialize;

const ALCHEMY_URL: &str = "https://base-mainnet.g.alchemy.com/v2/GXFdQW3k-NzViljQzTLnYH8svxlkFifs";

#[derive(Deserialize)]
struct AccountSnapshot {
    info: Option<AccountInfo>,
    #[allow(dead_code)]
    storage: RevmHashMap<U256, U256>,
}

#[derive(Deserialize)]
struct CacheSnapshot {
    #[allow(dead_code)]
    block_number: u64,
    #[allow(dead_code)]
    has_state_clear: bool,
    accounts: RevmHashMap<Address, AccountSnapshot>,
    codes: RevmHashMap<B256, Bytecode>,
}

fn hash_skeleton(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut skeleton = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        skeleton.push(op);
        i += 1;
        if (0x60..=0x7f).contains(&op) {
            i += (op - 0x5f) as usize;
        }
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    skeleton.hash(&mut hasher);
    hasher.finish()
}

fn rpc_call(method: &str, params: serde_json::Value) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let mut attempt = 0;
    loop {
        match ureq::post(ALCHEMY_URL).set("Content-Type", "application/json").send_json(&body) {
            Ok(resp) => match resp.into_json::<serde_json::Value>() {
                Ok(v) => return v,
                Err(e) => {
                    eprintln!("json parse error: {e}");
                }
            },
            Err(e) => {
                eprintln!("rpc error ({method}): {e}");
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let parse = |key: &str, default: usize| -> usize {
        args.windows(2)
            .find(|w| w[0] == format!("--{}", key))
            .and_then(|w| w[1].parse().ok())
            .unwrap_or(default)
    };
    let top = parse("top", 5);
    let sample = parse("sample-per-group", 50);
    let concurrency = parse("concurrency", 12);

    let bench_dir = Path::new("/home/ubuntu/sipeng/bench_data");
    let start: u64 = 38004930;
    let end: u64 = 38014927;

    eprintln!("Loading codes + account index from blocks {start}..{end}");
    let mut all_codes: HashMap<B256, Vec<u8>> = HashMap::new();
    let mut code_hash_to_addr: HashMap<B256, Address> = HashMap::new();
    let mut scanned = 0u64;
    for bn in start..end {
        let path = bench_dir.join(format!("states/{bn}.bin"));
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let snapshot: CacheSnapshot = match bincode::deserialize(&data) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for (hash, bytecode) in &snapshot.codes {
            all_codes.entry(*hash).or_insert_with(|| bytecode.original_byte_slice().to_vec());
        }
        for (addr, acct) in &snapshot.accounts {
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
        "Scanned {scanned} blocks: {} unique bytecodes, {} code_hash→addr mappings",
        all_codes.len(),
        code_hash_to_addr.len()
    );

    // Group by skeleton
    let mut skeleton_groups: HashMap<u64, Vec<B256>> = HashMap::new();
    for (hash, bytes) in &all_codes {
        if bytes.is_empty() {
            continue;
        }
        skeleton_groups.entry(hash_skeleton(bytes)).or_default().push(*hash);
    }
    let mut dup_groups: Vec<(u64, Vec<B256>)> =
        skeleton_groups.into_iter().filter(|(_, m)| m.len() >= 2).collect();
    dup_groups.sort_by_key(|(_, m)| std::cmp::Reverse(m.len()));

    eprintln!("Querying Base mainnet head block…");
    let head = eth_block_number();
    eprintln!("Head block: {head}");

    println!("=== Skeleton Group Deployment Time Span ===\n");
    println!("Head block: {head}");
    println!("Measurement window: {start}..{end}\n");

    for rank in 0..top.min(dup_groups.len()) {
        let (skel_hash, members) = &dup_groups[rank];
        let n = members.len();
        let bytecode_len = all_codes[&members[0]].len();

        // Pick members that have addresses in our cache
        let eligible: Vec<(B256, Address)> =
            members.iter().filter_map(|h| code_hash_to_addr.get(h).map(|a| (*h, *a))).collect();
        let take = sample.min(eligible.len());
        let sampled: Vec<(B256, Address)> = eligible.into_iter().take(take).collect();

        eprintln!(
            "\nGroup #{} (skel {:#x}): {} members, bytecode {} bytes — sampling {} with addresses",
            rank + 1,
            skel_hash,
            n,
            bytecode_len,
            sampled.len()
        );

        // Query deployment blocks in parallel
        let deploy_blocks: Arc<Mutex<Vec<(Address, Option<u64>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let counter = Arc::new(Mutex::new(0usize));
        let total = sampled.len();
        let chunks: Vec<Vec<(B256, Address)>> = {
            let mut v: Vec<Vec<(B256, Address)>> = (0..concurrency).map(|_| Vec::new()).collect();
            for (i, item) in sampled.into_iter().enumerate() {
                v[i % concurrency].push(item);
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

        let results = deploy_blocks.lock().unwrap().clone();
        let mut blocks: Vec<u64> = results.iter().filter_map(|(_, db)| *db).collect();
        blocks.sort();

        if blocks.is_empty() {
            println!("Group #{}: no deployment blocks resolved", rank + 1);
            continue;
        }

        let min_b = *blocks.first().unwrap();
        let max_b = *blocks.last().unwrap();
        let median_b = blocks[blocks.len() / 2];

        // Fetch timestamps
        let min_ts = eth_block_timestamp(min_b);
        let max_ts = eth_block_timestamp(max_b);
        let median_ts = eth_block_timestamp(median_b);
        let span_days = (max_ts - min_ts) as f64 / 86400.0;

        println!("-- Group #{} (skel {:#x}) --", rank + 1, skel_hash);
        println!("  members: {}  |  bytecode len: {}", n, bytecode_len);
        println!("  sampled: {} resolved / {} attempted", blocks.len(), total);
        println!("  deploy block  min={}  median={}  max={}", min_b, median_b, max_b);
        println!("  deploy ts     min={}  median={}  max={}", min_ts, median_ts, max_ts);
        println!("  span:         {:.1} days", span_days);
        // Pretty UTC dates
        println!("  min UTC: {}", fmt_ts(min_ts));
        println!("  max UTC: {}", fmt_ts(max_ts));
        println!();
    }
}

fn fmt_ts(ts: u64) -> String {
    // Seconds since epoch → YYYY-MM-DD HH:MM:SS UTC, minimal impl.
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
