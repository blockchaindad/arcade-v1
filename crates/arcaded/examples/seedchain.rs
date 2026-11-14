//! Copy blocks 0..n of a chain from one arcade's chain store (opened read only, safe while its
//! arcaded runs) into another home's, for local test nets: seedchain <from home> <to home> <chain key> <n>
//! `<from>` may instead be a file of raw blocks in hex, one per line from block 0 (fetched with getblock over an
//! arcade's RPC, which unseals blocks a store keeps only sealed).
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (from, to, key, n): (&str, &str, &str, u32) = (&a[1], &a[2], &a[3], a[4].parse().unwrap());
    let p = arcade_chain::params::by_key(key).expect("chain");
    if from.ends_with(".hex") {
        let text = std::fs::read_to_string(from).expect("hex file");
        std::fs::create_dir_all(to).unwrap();
        let dst = arcade_chaindb::ChainDb::open(std::path::Path::new(to).join("chain.mdbx")).expect("destination");
        let start = dst.tip(p.chain_no).ok().flatten().map(|t| t.height + 1).unwrap_or(0);
        let blocks: Vec<arcade_chain::Block> = text
            .lines()
            .skip(start as usize)
            .take((n - start) as usize)
            .map(|l| arcade_chain::Block::parse_with(&hex::decode(l.trim()).expect("hex"), &p.format).expect("block parses"))
            .collect();
        for batch in blocks.chunks(500) {
            dst.connect_blocks(p, batch).expect("connect");
        }
        return println!("{key}: blocks {start}..{n} copied from {from}; tip {:?}", dst.tip(p.chain_no).unwrap().map(|t| t.height));
    }
    let src = arcade_chaindb::ChainDb::open_readonly(std::path::Path::new(from).join("chain.mdbx")).expect("source");
    std::fs::create_dir_all(to).unwrap();
    let dst = arcade_chaindb::ChainDb::open(std::path::Path::new(to).join("chain.mdbx")).expect("destination");
    let start = dst.tip(p.chain_no).ok().flatten().map(|t| t.height + 1).unwrap_or(0);
    let mut h = start;
    while h < n {
        let mut batch = Vec::new();
        while h < n && batch.len() < 500 {
            let id = src.hash_at(p.chain_no, h).unwrap().expect("height in the source");
            let raw = src.block_raw(p.chain_no, &id).unwrap().expect("block in the source");
            batch.push(arcade_chain::Block::parse_with(&raw, &p.format).expect("block parses"));
            h += 1;
        }
        dst.connect_blocks(p, &batch).expect("connect");
    }
    println!("{key}: blocks {start}..{n} copied; tip {:?}", dst.tip(p.chain_no).unwrap().map(|t| t.height));
}
