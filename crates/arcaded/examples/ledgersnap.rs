//! Take the ledger snapshot for ARCADE's genesis (docs/ARCADECHAIN.md §15 step 19) from an arcade's store, read-only
//! (a running arcade's store can be read at the same time):
//!
//!   cargo run --release --example ledgersnap -- <arcade home>/chain.mdbx <chain key> <out.json>
//!
//! Prints the snapshot's hash (block 0's `genesis.ledger`) and how many rows it holds.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (path, key, out) = (&a[1], &a[2], &a[3]);
    let p = arcade_chain::params::by_key(key).expect("a chain key");
    let db = arcade_chaindb::ChainDb::open_readonly(path).expect("open the store read-only");
    let height = db.indexed_to(p.chain_no, "ledger.v1").expect("read").expect("the ledger index isn't on that chain here");
    let snap = arcade_ledger::snapshot::make(&db, p, height).expect("snapshot");
    std::fs::write(out, arcade_ledger::snapshot::bytes(&snap)).expect("write");
    println!(
        "{{\"hash\": \"{}\", \"chain\": \"{key}\", \"height\": {height}, \"tags\": {}, \"tokens\": {}, \"balances\": {}}}",
        hex::encode(arcade_ledger::snapshot::hash(&snap)),
        snap["tags"].as_array().map_or(0, |v| v.len()),
        snap["tokens"].as_array().map_or(0, |v| v.len()),
        snap["balances"].as_array().map_or(0, |v| v.len())
    );
}
