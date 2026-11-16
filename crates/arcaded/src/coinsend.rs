//! Sending a chain's own coin from this wallet (wallet.send), on any chain: the home chain and
//! installed chains (coins from our address index, broadcast as tx.broadcast does) and borrowed
//! ones (crate::borrow::send). Each chain's fee and dust rules, and the addresses a wallet key
//! has on a chain, live here too.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use arcade_ledger::address::{base58check, bech32, decode_base58check, hash160, p2pkh_script, p2wpkh_script, script_for_address};
use arcade_wallet::secp256k1::PublicKey;
use arcade_wallet::tx::Utxo;
use serde_json::{json, Value};

type R = Result<Value, String>;

/// A chain's fee rules, in satoshis: the rate used when the network gives no estimate, the
/// lowest rate ever used, and the smallest output worth making.
pub struct FeePolicy {
    pub fallback_per_kb: u64,
    pub floor_per_kb: u64,
    pub dust: u64,
}

pub fn fee_policy(p: &ChainParams) -> FeePolicy {
    match p.key {
        // DigiByte Core (v8.22.2 and v9.26.x, src/policy/policy.h, src/wallet/wallet.h):
        // minimum relay fee 100,000 sat/kB (0.001 DGB/kB); the wallet's fallback rate
        // 0.01 DGB/kB, which the old arcade uses too; dust relay fee 30,000 sat/kB, so a P2PKH
        // output under (34 + 148) x 30 = 5,460 sat is dust.
        "dgb" | "dgbtest" => FeePolicy { fallback_per_kb: 1_000_000, floor_per_kb: 100_000, dust: 5_460 },
        // The Dogecoin family (1 coin/kB rules), as the arcade's claims use them.
        "peptest" | "pep" | "doge" | "flop" | "wjk" | "kot" | "shic" | "lky" => {
            FeePolicy { fallback_per_kb: crate::claims::FEE_PER_KB, floor_per_kb: 1_000, dust: crate::claims::DUST }
        }
        // The ARCADE chain: its pool's floor and the chain's own smallest output (a rule
        // there, not a habit). Twice the floor, so a signature a byte longer still clears it.
        "arc" | "arctest" => FeePolicy { fallback_per_kb: 2 * arcade_arc::params::MIN_FEE_PER_KB, floor_per_kb: arcade_arc::params::MIN_FEE_PER_KB, dust: arcade_arc::params::DUST },
        // Bells 3 (the original arcade's settings): 0.0001 BEL/kB, ten times its minimum relay fee; no soft dust;
        // change from 0.0001 BEL.
        "bel" => FeePolicy { fallback_per_kb: 10_000, floor_per_kb: 1_000, dust: 10_000 },
        // Junkcoin Core 4.0.3 (Litecoin 0.21's policy, per byte): minimum relay fee 1,000 sat/kB, the wallet's minimum
        // 10,000 and fallback 200,000 sat/kB (wallet.h); dust relay fee 30,000 sat/kB, so a P2PKH output under
        // (34 + 148) x 30 = 5,460 sat is dust.
        "jkc" => FeePolicy { fallback_per_kb: 200_000, floor_per_kb: 10_000, dust: 5_460 },
        _ => FeePolicy { fallback_per_kb: 20_000, floor_per_kb: 1_000, dust: 1_000 },
    }
}

/// Chains where the wallet receives at a native SegWit (bech32) address. DigiByte's "D..."
/// addresses are the same strings as Dogecoin's (version byte 30, and V1.0 derives both from
/// one key), so DigiByte receives at "dgb1..." instead; its "D..." address is still watched.
pub fn segwit_receive(p: &ChainParams) -> bool {
    matches!(p.key, "dgb" | "dgbtest") && p.bech32_hrp.is_some() && p.format.segwit
}

/// A wallet key's addresses on a chain, the receiving one first: (address, script).
pub fn key_scripts(p: &ChainParams, pk: &PublicKey) -> Vec<(String, Vec<u8>)> {
    let h = hash160(&pk.serialize());
    let pkh = (base58check(p.p2pkh_version, &h), p2pkh_script(&h));
    match p.bech32_hrp.filter(|_| segwit_receive(p)) {
        Some(hrp) => vec![(bech32::segwit_v0(hrp, &h), p2wpkh_script(&h)), pkh],
        None => vec![pkh],
    }
}

/// A word of caution about where a payment goes, when the address could be meant for another
/// chain: a DigiByte "D..." address is also a Dogecoin address.
pub fn warning(p: &ChainParams, to: &str) -> Option<String> {
    if matches!(p.key, "dgb") && decode_base58check(to).is_some_and(|(v, _)| v == p.p2pkh_version) {
        return Some("D... addresses look the same on DigiByte and Dogecoin. Make sure whoever you're paying uses DigiByte at this address; dgb1... addresses can't be mixed up.".into());
    }
    None
}

/// wallet.send: pay `amount` (satoshis) to `to` on a chain from one of this wallet's keys,
/// signed here. With broadcast false it is only built and shown (fee, size, warning).
pub fn send(node: &Node, key: Option<&str>, to: &str, amount: u64, from: Option<&str>, broadcast: bool) -> R {
    let from = from.map(str::trim).filter(|f| !f.is_empty());
    let p: &'static ChainParams = match key.filter(|k| !k.is_empty()) {
        Some(k) => crate::chains::find(k).ok_or_else(|| format!("unknown chain {k}"))?,
        None => node.params,
    };
    let warn = warning(p, to);
    if crate::borrow::is_borrowed(p.key) {
        let mut v = crate::borrow::send(node, p.key, to, amount, from, broadcast)?;
        v["chain"] = json!(p.key);
        v["warning"] = json!(warn);
        return Ok(v);
    }
    // The ARCADE chain's waiting transactions are in its own pool, not the store's.
    let arc = crate::arcsvc::runs(p);
    let waiting_spend = |op: &arcade_chain::OutPoint| -> Result<bool, String> {
        if arc {
            return Ok(crate::arcsvc::spender(op).is_some() || !crate::arcsvc::unspent(op));
        }
        Ok(node.chain.mempool_spender(p.chain_no, op).map_err(|e| e.to_string())?.is_some())
    };
    if !crate::chains::kept(node, p) {
        return Err(format!("{} is neither installed nor borrowed here", p.name));
    }
    if !node.chain.addresses_on(p.chain_no).map_err(|e| e.to_string())? {
        return Err(format!("{} keeps no address index here, so its coins can't be found to send", p.name));
    }
    // An @name instead of an address: its key's address on this chain, where that is sure to be theirs.
    let named = to_of_tag(node, p, to)?;
    let to = named.as_deref().unwrap_or(to);
    let to_script = script_for_address(p, to).ok_or_else(|| format!("{to} is not a {} address", p.name))?;
    let tip = node.chain.tip(p.chain_no).map_err(|e| e.to_string())?.map(|t| t.height).unwrap_or(0);
    // The key with the most to spend.
    let mut keys = node.app.tag_keys(p.chain_no).map_err(|e| e.to_string())?;
    if keys.is_empty() && node.app.has_wallet().map_err(|e| e.to_string())? {
        keys.push(node.app.tag_key(p, 0).map_err(|e| e.to_string())?.0);
    }
    let mut best: Option<(u32, Vec<(String, Vec<u8>)>, Vec<Utxo>, u64)> = None;
    for k in keys {
        let Ok(pk) = hex::decode(&k.pubkey).map_err(|_| ()).and_then(|b| PublicKey::from_slice(&b).map_err(|_| ())) else { continue };
        let scripts = key_scripts(p, &pk);
        let mut coins = Vec::new();
        for (_, sc) in &scripts {
            for (op, value, _h) in node.chain.coins_for_script(p, sc).map_err(|e| e.to_string())? {
                // Not spent by a waiting transaction, and not an immature coinbase.
                if waiting_spend(&op)? {
                    continue;
                }
                if let Some(c) = node.chain.coin(p, &op).map_err(|e| e.to_string())? {
                    if c.coinbase && c.height + p.coinbase_maturity > tip {
                        continue;
                    }
                }
                coins.push(Utxo { outpoint: op, value, script: sc.clone() });
            }
        }
        let coins = crate::ordsafe::keep_free(node, p, coins);
        let total: u64 = coins.iter().map(|u| u.value).sum();
        let picked = from.is_some_and(|f| scripts.iter().any(|(a, _)| a == f));
        if picked {
            best = Some((k.index, scripts, coins, u64::MAX));
            break;
        }
        if best.as_ref().is_none_or(|b| total > b.3) {
            best = Some((k.index, scripts, coins, total));
        }
    }
    // An imported wallet picked by its address spends its own coins, change back to it.
    let imported = match from {
        Some(f) if !best.as_ref().is_some_and(|b| b.1.iter().any(|(a, _)| a == f)) => {
            let (_, sc, sk, kind) = crate::importsvc::wallets_on(node, p.key).into_iter().find(|w| w.0 == f).ok_or("that address is not one of this wallet's (own or imported) on this chain")?;
            let keyhash = (sc.len() == 25 && sc[0] == 0x76 && sc[1] == 0xa9 && kind == "legacy") || (sc.len() == 22 && sc[0] == 0 && sc[1] == 20);
            let pk = PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
            if !keyhash || !key_scripts(p, &pk).iter().any(|(_, s)| *s == sc) {
                return Err("this imported wallet's kind can't be spent here yet (only legacy compressed and p2wpkh)".into());
            }
            let mut coins = Vec::new();
            for (op, value, _h) in node.chain.coins_for_script(p, &sc).map_err(|e| e.to_string())? {
                if waiting_spend(&op)? {
                    continue;
                }
                if let Some(c) = node.chain.coin(p, &op).map_err(|e| e.to_string())? {
                    if c.coinbase && c.height + p.coinbase_maturity > tip {
                        continue;
                    }
                }
                coins.push(Utxo { outpoint: op, value, script: sc.clone() });
            }
            let coins = crate::ordsafe::keep_free(node, p, coins);
            Some((sk, vec![(f.to_string(), sc)], coins))
        }
        _ => None,
    };
    let (sk, scripts, coins) = match imported {
        Some(x) => x,
        None => {
            let (index, scripts, coins, _) = best.ok_or("this wallet has no address on that chain")?;
            (node.app.tag_key(p, index).map_err(|e| e.to_string())?.1, scripts, coins)
        }
    };
    let fp = fee_policy(p);
    let tx = arcade_wallet::tx::build_send_keyhash(p, &to_script, amount, &sk, &coins, &scripts[0].1, fp.fallback_per_kb.max(fp.floor_per_kb), fp.dust, arcade_app::now() as u32)?;
    let ins: u64 = tx.inputs.iter().filter_map(|i| coins.iter().find(|c| c.outpoint == i.prevout).map(|c| c.value)).sum();
    let fee = ins.saturating_sub(tx.outputs.iter().map(|o| o.value).sum());
    let vsize = (tx.encode_legacy().len() * 3 + tx.raw.len()).div_ceil(4);
    let mut out = json!({"chain": p.key, "txid": tx.txid.to_hex(), "from": scripts[0].0, "to": to, "amount": amount, "fee": fee,
                         "size": tx.raw.len(), "vsize": vsize, "sent": false, "warning": warn});
    if broadcast {
        let r = if arc {
            let (id, fee) = crate::arcsvc::send_raw(&tx.raw)?;
            json!({"txid": id.to_hex(), "fee": fee, "size": tx.raw.len()})
        } else {
            crate::sitesvc::broadcast(node, &json!({"hex": hex::encode(&tx.raw), "chain": p.key}))?
        };
        out["sent"] = json!(true);
        out["broadcast"] = r;
    }
    Ok(out)
}


/// `arc.blob_publish`: put `bytes` on the ARCADE chain as a blob (arcade-arc `blob.rs`). See [`publish_blobs`].
pub fn publish_blob(node: &Node, bytes: &[u8], kind: &str, class: arcade_arc::blob::Class, encoding: arcade_arc::blob::Encoding, send: bool) -> Result<Value, String> {
    let mut v = publish_blobs(node, &[(bytes.to_vec(), kind.to_string(), class, encoding)], send)?;
    // One blob: its own fields at the top, as before.
    if let Some(first) = v["blobs"].get(0).cloned() {
        for k in ["blob", "root", "bytes", "type", "class", "new"] {
            v[k] = first[k].clone();
        }
    }
    Ok(v)
}

/// `arc.blob_publish_many`: blobs in one transaction from this arcade's wallet: a record each, change back, paying
/// the wallet's rate for its own bytes and 0.01 ARCADE a kB of every blob the chain doesn't hold yet; into the pool with
/// the blobs' bytes. At most 4 MB of new blobs (one block's budget) and the transaction under 100 kB. Without `send`,
/// only what it would be.
pub fn publish_blobs(node: &Node, items: &[(Vec<u8>, String, arcade_arc::blob::Class, arcade_arc::blob::Encoding)], send: bool) -> Result<Value, String> {
    use arcade_arc::blob::{self, Record};
    use arcade_wallet::tx::{fee_for_size, sign_keyhash, verify_keyhash, TxOutSpec};
    let p = crate::arcsvc::defined().filter(|p| crate::arcsvc::runs(p)).ok_or("the ARCADE chain isn't running here")?;
    let net = crate::arcsvc::svc_net().ok_or("the ARCADE chain isn't running here")?;
    if items.is_empty() {
        return Err("no blobs".into());
    }
    let mut recs = Vec::new();
    let (mut new_bytes, mut seen) = (0u64, std::collections::HashSet::new());
    for (bytes, kind, class, encoding) in items {
        if bytes.is_empty() || bytes.len() > blob::MAX_BLOB_BYTES || kind.len() > blob::MAX_TYPE {
            return Err(format!("a blob is 1 to {} bytes, its type at most {} characters", blob::MAX_BLOB_BYTES, blob::MAX_TYPE));
        }
        let rec = Record::of(bytes, *class, *encoding, kind);
        let id = rec.id(&net);
        let new = !crate::arcsvc::holds_blob(&id) && seen.insert(id);
        if new || *class == blob::Class::Boost {
            new_bytes += bytes.len() as u64;
        }
        recs.push((rec, id, new));
    }
    if recs.iter().filter(|r| r.2).map(|r| r.0.len).sum::<u64>() > blob::MAX_BLOB_BYTES as u64 {
        return Err("more than one block's 4 MB of new blobs: split them into several".into());
    }
    let tip = node.chain.tip(p.chain_no).map_err(|e| e.to_string())?.map(|t| t.height).unwrap_or(0);
    // Signing as a tag (crate::arcledger::with_sign_as): that tag's key pays; else the wallet's first key, as always.
    let (pk, sk) = if crate::arcledger::sign_as().is_some() {
        let (_, _, sk) = crate::arcledger::sender(node, p)?;
        (PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk), sk)
    } else {
        let (k, sk) = node.app.tag_key(p, 0).map_err(|e| e.to_string())?;
        (PublicKey::from_slice(&hex::decode(&k.pubkey).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?, sk)
    };
    let scripts = key_scripts(p, &pk);
    let mut coins = Vec::new();
    for (_, sc) in &scripts {
        for (op, value, _h) in node.chain.coins_for_script(p, sc).map_err(|e| e.to_string())? {
            // Not spent by a waiting transaction, nor on the chain itself (its store copy follows a moment later).
            if crate::arcsvc::spender(&op).is_some() || !crate::arcsvc::unspent(&op) {
                continue;
            }
            if let Some(c) = node.chain.coin(p, &op).map_err(|e| e.to_string())? {
                if c.coinbase && c.height + p.coinbase_maturity > tip {
                    continue;
                }
            }
            coins.push(Utxo { outpoint: op, value, script: sc.clone() });
        }
    }
    coins.sort_by(|a, b| b.value.cmp(&a.value));
    let fp = fee_policy(p);
    let change_script = scripts[0].1.clone();
    let mut out_scripts: Vec<usize> = recs.iter().map(|r| r.0.script().len()).collect();
    out_scripts.push(change_script.len());
    let blob_fee = blob::fee_for(new_bytes);
    let fee_of = |n: usize| fee_for_size(p, arcade_wallet::tx::estimate_size(n, &out_scripts) as u64, fp.fallback_per_kb) + blob_fee;
    let (mut chosen, mut have) = (Vec::new(), 0u64);
    for c in coins {
        have += c.value;
        chosen.push(c);
        if have >= fee_of(chosen.len()) + fp.dust {
            break;
        }
    }
    let fee = fee_of(chosen.len());
    if have < fee + fp.dust {
        return Err(format!("not enough ARCADE: have {have}, need {} (the blob fee is {blob_fee})", fee + fp.dust));
    }
    let mut outs: Vec<TxOutSpec> = recs.iter().map(|r| TxOutSpec { value: 0, script: r.0.script() }).collect();
    outs.push(TxOutSpec { value: have - fee, script: change_script });
    let tx = sign_keyhash(p, 1, &chosen, &outs, &sk, arcade_app::now() as u32)?;
    if !verify_keyhash(p, &tx, &chosen) {
        return Err("the signed transaction didn't verify".into());
    }
    if tx.raw.len() > arcade_arc::coins::MAX_ONE_TX_BYTES {
        return Err(format!("{} blobs make a transaction over 100 kB: send fewer at once", recs.len()));
    }
    let blobs: Vec<Value> = recs
        .iter()
        .zip(items)
        .map(|((rec, id, new), it)| json!({"blob": hex::encode(id), "root": hex::encode(rec.root), "bytes": rec.len, "type": it.1, "class": it.2 as u8, "new": new}))
        .collect();
    let mut out = json!({"blobs": blobs, "blob_fee": blob_fee, "fee": fee, "txid": tx.txid.to_hex(), "hex": hex::encode(&tx.raw), "size": tx.raw.len(), "sent": false});
    if send {
        for (bytes, ..) in items {
            crate::arcblob::hold(&net, bytes.clone()).ok_or("no room for more waiting blobs")?;
        }
        let (txid, fee) = crate::arcsvc::send_raw(&tx.raw)?;
        out["sent"] = json!(true);
        out["txid"] = json!(txid.to_hex());
        out["fee"] = json!(fee);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_chain::params::{DIGIBYTE_MAIN, DOGECOIN_MAIN, PEPECOIN_TEST};

    #[test]
    fn digibyte_rules_and_addresses() {
        let fp = fee_policy(&DIGIBYTE_MAIN);
        assert_eq!((fp.fallback_per_kb, fp.floor_per_kb, fp.dust), (1_000_000, 100_000, 5_460));
        let sk = arcade_wallet::SecretKey::from_slice(&[7u8; 32]).unwrap();
        let pk = PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
        let dgb = key_scripts(&DIGIBYTE_MAIN, &pk);
        let doge = key_scripts(&DOGECOIN_MAIN, &pk);
        // DigiByte receives at dgb1...; its D... address is the very string Dogecoin shows.
        assert!(dgb[0].0.starts_with("dgb1q"));
        assert_eq!(dgb[1].0, doge[0].0);
        assert_eq!(doge.len(), 1);
        assert!(warning(&DIGIBYTE_MAIN, &dgb[1].0).is_some());
        assert!(warning(&DIGIBYTE_MAIN, &dgb[0].0).is_none());
        assert!(warning(&DOGECOIN_MAIN, &doge[0].0).is_none());
        assert_eq!(key_scripts(&PEPECOIN_TEST, &pk).len(), 1);
    }
}

thread_local! {
    /// The send being made says it knows an @tag changed owner (its request's `owner_changed_ok`).
    pub(crate) static OWNER_CHANGE_OK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `to` as an @name (with or without the @, when it isn't an address of `p`): the claim's address on the home chain,
/// with its key hash under `p`'s version byte. Only when `p` derives keys as the home chain does (the same BIP44 coin
/// type: the ARCADE testnet beside Pepecoin testnet), so the address is one the name's own wallet holds; elsewhere the
/// sender is asked for their address. None when `to` isn't a name.
fn to_of_tag(node: &Node, p: &ChainParams, to: &str) -> Result<Option<String>, String> {
    let t = to.strip_prefix('@').unwrap_or(to).to_lowercase();
    if p.chain_no == node.params.chain_no || script_for_address(p, to).is_some() {
        return Ok(None);
    }
    if !(2..=24).contains(&t.len()) || !t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Ok(None);
    }
    // On the ARCADE chain, its own ledger's @tag first (§15 step 7); a released tag that changed hands lately warns,
    // and the send is refused unless it says it knows (`owner_changed_ok`).
    if crate::arcsvc::runs(p) {
        if let Some((address, warn)) = crate::arcledger::tag_owner(node, &t) {
            if let Some(w) = warn {
                if !OWNER_CHANGE_OK.with(|c| c.get()) {
                    return Err(format!("{w}: send again with owner_changed_ok if you mean the new owner"));
                }
            }
            return Ok(Some(address));
        }
    }
    let row = arcade_ledger::index::tag_row(&node.chain, node.params.chain_no, &t).map_err(|e| e.to_string())?
        .ok_or_else(|| format!("@{t} is not claimed"))?;
    if p.bip44_coin_type != node.params.bip44_coin_type {
        return Err(format!("@{t}'s {} address can't be worked out from their name here: paste their address", p.name));
    }
    let (_, hash) = decode_base58check(&row.address).filter(|(_, h)| h.len() == 20).ok_or_else(|| format!("@{t}'s address doesn't read"))?;
    Ok(Some(base58check(p.p2pkh_version, &hash)))
}
