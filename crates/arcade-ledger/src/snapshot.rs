//! The ledger snapshot carried into ARCADE's genesis (docs/ARCADECHAIN.md §15 step 19): the home chain's @tags, tokens
//! and token balances at an announced height, by key hash, so the ARCADE chain's ledger starts where the home chain's
//! stood. Apps stay where they were inscribed.
//!
//! The snapshot is JSON in one canonical form (keys sorted, rows sorted): `{"chain", "height", "block", "tags":
//! [[tag, kind, hash160]], "tokens": [[id, definition]], "balances": [[kind, hash160, id, [free, reserved, offered,
//! accepted]]]}`, kind 0 a key hash (p2pkh), 1 a script hash (p2sh). Its SHA-256 is named in block 0 (`Genesis::
//! ledger`), and every arcade imports the same rows before block 1, under the ARCADE chain's address versions.

use crate::address::{base58check, decode_base58check};
use crate::engine::{BALANCE, PROPERTY};
use crate::index::{TAG, TAG_BY_ADDRESS};
use arcade_chain::params::ChainParams;
use arcade_store::{KeyBuf, Result, StoreError};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn kind_and_hash(p: &ChainParams, address: &str) -> Option<(u8, String)> {
    let (v, h) = decode_base58check(address)?;
    if h.len() != 20 {
        return None;
    }
    let kind = if v == p.p2pkh_version {
        0
    } else if v == p.p2sh_version {
        1
    } else {
        return None;
    };
    Some((kind, hex::encode(h)))
}

fn address_of(p: &ChainParams, kind: u64, h160: &str) -> Option<String> {
    let h = hex::decode(h160).ok().filter(|h| h.len() == 20)?;
    Some(base58check(if kind == 0 { p.p2pkh_version } else { p.p2sh_version }, &h))
}

/// The snapshot of chain `p`'s ledger as it stands in `db` (taken when its index is at `height`).
pub fn make(db: &arcade_chaindb::ChainDb, p: &ChainParams, height: u32) -> Result<Value> {
    let c = p.chain_no;
    let pre = KeyBuf::new().u16(c).done();
    let st = db.store(c);
    let mut tags = Vec::new();
    for (k, v) in st.read(|r| r.scan_prefix(TAG, &pre))? {
        let tag = String::from_utf8_lossy(&k[2..]).into_owned();
        let row: Value = serde_json::from_slice(&v).unwrap_or(Value::Null);
        if let Some((kind, h)) = row["a"].as_str().and_then(|a| kind_and_hash(p, a)) {
            tags.push(json!([tag, kind, h]));
        }
    }
    let mut tokens = Vec::new();
    for (k, v) in st.read(|r| r.scan_prefix(PROPERTY, &pre))? {
        let id = u32::from_be_bytes(k[2..6].try_into().map_err(|_| StoreError::Other("a token key".into()))?);
        tokens.push(json!([id, serde_json::from_slice::<Value>(&v).unwrap_or(Value::Null)]));
    }
    let mut balances = Vec::new();
    for (k, v) in st.read(|r| r.scan_prefix(BALANCE, &pre))? {
        // C | address | 00 | id
        let Some(cut) = k.len().checked_sub(5) else { continue };
        let address = String::from_utf8_lossy(&k[2..cut]).into_owned();
        let id = u32::from_be_bytes(k[cut + 1..].try_into().map_err(|_| StoreError::Other("a balance key".into()))?);
        let amounts: Vec<u64> = (0..4).map(|i| v.get(i * 8..i * 8 + 8).map_or(0, |b| u64::from_be_bytes(b.try_into().unwrap()))).collect();
        if amounts.iter().all(|a| *a == 0) {
            continue;
        }
        if let Some((kind, h)) = kind_and_hash(p, &address) {
            balances.push(json!([kind, h, id, amounts]));
        }
    }
    let sort = |v: &mut Vec<Value>| v.sort_by_key(|x| x.to_string());
    sort(&mut tags);
    sort(&mut balances);
    tokens.sort_by_key(|t| t[0].as_u64());
    let block = db.hash_at(c, height)?.map(|h| h.to_hex());
    Ok(json!({"chain": p.key, "height": height, "block": block, "tags": tags, "tokens": tokens, "balances": balances}))
}

/// The snapshot's bytes in its one form (serde_json's maps keep keys sorted).
pub fn bytes(snap: &Value) -> Vec<u8> {
    serde_json::to_vec(snap).expect("JSON")
}

pub fn hash(snap: &Value) -> [u8; 32] {
    Sha256::digest(bytes(snap)).into()
}

/// What an import wrote.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Imported {
    pub tags: usize,
    pub tokens: usize,
    pub balances: usize,
}

/// Write the snapshot's rows into chain `to`'s ledger tables, under its address versions (before block 1: nothing is
/// journaled, as block 0 changes nothing).
pub fn import(db: &arcade_chaindb::ChainDb, to: &ChainParams, snap: &Value) -> Result<Imported> {
    let c = to.chain_no;
    let k = || KeyBuf::new().u16(c);
    let mut n = Imported::default();
    db.store(c).write(|w| {
        for (i, t) in snap["tags"].as_array().into_iter().flatten().enumerate() {
            let (Some(tag), Some(kind), Some(h)) = (t[0].as_str(), t[1].as_u64(), t[2].as_str()) else { continue };
            let Some(a) = address_of(to, kind, h) else { continue };
            w.put(TAG, &k().bytes(tag.as_bytes()).done(), json!({"a": a, "t": "snapshot", "h": 0, "p": i}).to_string().as_bytes())?;
            w.put(TAG_BY_ADDRESS, &k().bytes(a.as_bytes()).done(), tag.as_bytes())?;
            n.tags += 1;
        }
        for t in snap["tokens"].as_array().into_iter().flatten() {
            let Some(id) = t[0].as_u64() else { continue };
            w.put(PROPERTY, &k().u32(id as u32).done(), t[1].to_string().as_bytes())?;
            n.tokens += 1;
        }
        for b in snap["balances"].as_array().into_iter().flatten() {
            let (Some(kind), Some(h), Some(id)) = (b[0].as_u64(), b[1].as_str(), b[2].as_u64()) else { continue };
            let Some(a) = address_of(to, kind, h) else { continue };
            let mut v = Vec::with_capacity(32);
            for i in 0..4 {
                v.extend_from_slice(&b[3][i].as_u64().unwrap_or(0).to_be_bytes());
            }
            w.put(BALANCE, &k().bytes(a.as_bytes()).u8(0).u32(id as u32).done(), &v)?;
            n.balances += 1;
        }
        Ok(())
    })?;
    Ok(n)
}
