mod bench_common;

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use revm::{
    bytecode::Bytecode,
    context::{BlockEnv, TxEnv},
    database::{CacheDB, EmptyDB},
    primitives::{keccak256, Address, Bytes, HashMap as RevmHashMap, TxKind, U256},
    state::AccountInfo,
};

// ── Constants ────────────────────────────────────────────────────────────────

const AIRDROP: Address = Address::new([0xdd; 20]);
const AIRDROP_TOKEN: Address = Address::new([0xaa; 20]);
const SENDER: Address = Address::new([
    0x89, 0xd5, 0xe7, 0x2a, 0x8a, 0x4a, 0x03, 0x30, 0xa6, 0x5b, 0xbc, 0xef, 0x30, 0x32, 0xbe,
    0x2f, 0x72, 0x82, 0x64, 0xa8,
]);

// ── ERC20 storage helpers ────────────────────────────────────────────────────

fn erc20_balance_slot(addr: Address) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(addr.as_slice());
    U256::from_be_bytes(keccak256(buf).0)
}

fn erc20_allowance_slot(owner: Address, spender: Address) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(owner.as_slice());
    buf[63] = 1;
    let inner = keccak256(buf);
    let mut buf2 = [0u8; 64];
    buf2[12..32].copy_from_slice(spender.as_slice());
    buf2[32..64].copy_from_slice(inner.as_slice());
    U256::from_be_bytes(keccak256(buf2).0)
}

// ── Setup ────────────────────────────────────────────────────────────────────

fn load_hex_file(name: &str) -> Bytecode {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../data")
        .join(name);
    let hex_str = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {name}: {e}"))
        .trim()
        .to_string();
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(&hex_str);
    let bytes = hex::decode(hex_str).expect("invalid hex");
    Bytecode::new_raw(Bytes::from(bytes))
}

fn make_airdrop_calldata(token: Address, recipients: &[Address], amount_each: U256) -> Bytes {
    let n = recipients.len();
    let total = amount_each * U256::from(n);

    let mut data = Vec::with_capacity(4 + 4 * 32 + 2 * (32 + n * 32));
    data.extend_from_slice(&[0xcc, 0xb9, 0x8f, 0xfc]);

    let mut padded = [0u8; 32];
    padded[12..32].copy_from_slice(token.as_slice());
    data.extend_from_slice(&padded);

    data.extend_from_slice(&U256::from(4 * 32).to_be_bytes::<32>());
    let recipients_section = 32 + n * 32;
    data.extend_from_slice(&U256::from(4 * 32 + recipients_section).to_be_bytes::<32>());
    data.extend_from_slice(&total.to_be_bytes::<32>());

    data.extend_from_slice(&U256::from(n).to_be_bytes::<32>());
    for &r in recipients {
        let mut p = [0u8; 32];
        p[12..32].copy_from_slice(r.as_slice());
        data.extend_from_slice(&p);
    }

    data.extend_from_slice(&U256::from(n).to_be_bytes::<32>());
    for _ in 0..n {
        data.extend_from_slice(&amount_each.to_be_bytes::<32>());
    }

    Bytes::from(data)
}

fn build_fixture(recipients: usize) -> bench_common::Fixture {
    let mut db = CacheDB::new(EmptyDB::new());

    // Deploy airdrop contract
    let airdrop_code = load_hex_file("airdrop.rt.hex");
    let airdrop_hash = airdrop_code.hash_slow();
    db.insert_account_info(
        AIRDROP,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash: airdrop_hash,
            code: Some(airdrop_code),
        },
    );
    // Set owner = SENDER (OZ v5 ERC-7201)
    let oz_owner_slot = U256::from_str_radix(
        "9016d09d72d40fdae2fd8ceac6b6234c7706214fd39c1cd1e609a0528c199300",
        16,
    )
    .unwrap();
    db.insert_account_storage(AIRDROP, oz_owner_slot, U256::from_be_slice(SENDER.as_slice()))
        .unwrap();

    // Deploy ERC20 token (use erc20_transfer.rt.hex as a simple ERC20)
    // Storage layout: balanceOf at slot 0, allowance at slot 1 (standard Solidity)
    let token_code = load_hex_file("erc20_transfer.rt.hex");
    let token_hash = token_code.hash_slow();
    db.insert_account_info(
        AIRDROP_TOKEN,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash: token_hash,
            code: Some(token_code),
        },
    );

    // Fund SENDER
    let huge = U256::from(10u64).pow(U256::from(30));
    db.insert_account_info(
        SENDER,
        AccountInfo {
            balance: U256::from(10u64).pow(U256::from(25)),
            nonce: 0,
            code_hash: revm::primitives::KECCAK_EMPTY,
            code: None,
        },
    );
    db.insert_account_storage(AIRDROP_TOKEN, erc20_balance_slot(SENDER), huge)
        .unwrap();
    db.insert_account_storage(
        AIRDROP_TOKEN,
        erc20_allowance_slot(SENDER, AIRDROP),
        U256::MAX,
    )
    .unwrap();

    // Generate recipients
    let recipients: Vec<Address> = (0..recipients)
        .map(|i| {
            let mut bytes = [0u8; 20];
            bytes[0] = 0x10;
            bytes[16..20].copy_from_slice(&(i as u32).to_be_bytes());
            Address::from(bytes)
        })
        .collect();
    let amount_each = U256::from(1_000_000_000_000_000u128);
    let calldata = make_airdrop_calldata(AIRDROP_TOKEN, &recipients, amount_each);

    let mut block = BlockEnv::default();
    block.number = U256::from(1);
    block.timestamp = U256::from(1000);
    block.gas_limit = 1_000_000_000;
    block.basefee = 1;
    block.set_blob_excess_gas_and_price(
        0,
        revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
    );

    let tx = TxEnv {
        tx_type: 0,
        caller: SENDER,
        gas_limit: 100_000_000,
        gas_price: 1,
        kind: TxKind::Call(AIRDROP),
        value: U256::ZERO,
        data: calldata,
        nonce: 0,
        chain_id: Some(1),
        access_list: Default::default(),
        gas_priority_fee: None,
        blob_hashes: Vec::new(),
        max_fee_per_blob_gas: 0,
        authorization_list: Vec::new(),
    };

    bench_common::Fixture::from_db(db, block, tx).expect("failed to build airdrop fixture")
}

// ── Benchmark ────────────────────────────────────────────────────────────────

fn bench_airdrop_n(c: &mut Criterion, n: usize) {
    let fixture = build_fixture(n);
    let group_name = format!("airdrop_{n}_recipients");

    let mut group = c.benchmark_group(&group_name);
    group.bench_function("plain_execution", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let result = fixture.run_plain().expect("plain execution failed");
                total += start.elapsed();
                black_box(result);
            }
            total
        });
    });
    group.bench_function("jit_optimized", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let result = fixture.run_jit().expect("jit execution failed");
                total += start.elapsed();
                black_box(result);
            }
            total
        });
    });
    group.finish();
}

pub fn bench_airdrop_200(c: &mut Criterion) { bench_airdrop_n(c, 200); }
pub fn bench_airdrop_2000(c: &mut Criterion) { bench_airdrop_n(c, 2000); }
pub fn bench_airdrop_20000(c: &mut Criterion) { bench_airdrop_n(c, 20000); }

criterion_group!(benches, bench_airdrop_200, bench_airdrop_2000, bench_airdrop_20000);
criterion_main!(benches);
