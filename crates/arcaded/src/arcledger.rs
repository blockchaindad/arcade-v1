//! The arcade ledger on the ARCADE chain (docs/ARCADECHAIN.md §15 step 7): the same `arcd` records as on the home chain
//! (tags, profiles, follows, tokens, orders, messages), indexed by the same engine (crate::arcsvc adds it), sent from
//! the wallet's tag key on ARCADE with ARCADE's coins, fees and pool. A record of up to 1 kB is one OP_RETURN output.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use serde_json::{json, Value};

/// The ARCADE chain running here.
pub fn chain() -> Result<&'static ChainParams, String> {
    crate::arcsvc::defined().filter(|p| crate::arcsvc::runs(p)).ok_or_else(|| "the ARCADE chain isn't running here".to_string())
}

/// The wallet's key on ARCADE that holds an @tag there (else its first tag key): (address, tag, secret key).
pub fn sender(node: &Node, p: &'static ChainParams) -> Result<(String, Option<String>, arcade_wallet::SecretKey), String> {
    for k in node.app.tag_keys(p.chain_no).map_err(|e| e.to_string())? {
        if let Ok(Some(t)) = arcade_ledger::index::tag_of(&node.chain, p.chain_no, &k.address) {
            let (k, sk) = node.app.tag_key(p, k.index).map_err(|e| e.to_string())?;
            return Ok((k.address, Some(t), sk));
        }
    }
    let (k, sk) = node.app.tag_key(p, 0).map_err(|e| e.to_string())?;
    Ok((k.address, None, sk))
}

/// One `arcd` record from the sender's key on ARCADE: `payload` is the whole record after the marker (version, type,
/// data; tags::claim_payload and the like make it whole). Its txid and fee.
pub fn send(node: &Node, payload: &[u8]) -> Result<(String, u64), String> {
    let p = chain()?;
    let (_, _, sk) = sender(node, p)?;
    let pk = arcade_wallet::secp256k1::PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
    let script = arcade_wallet::keys::p2pkh_script_for(&pk);
    let coins = crate::claims::arc_coins(node, p, &script);
    let fee_per_kb = 2 * arcade_arc::params::MIN_FEE_PER_KB;
    let tx = arcade_wallet::tx::build_arcade_with(p, payload, vec![], &sk, &coins, fee_per_kb, arcade_arc::params::DUST,
        arcade_app::now() as u32, arcade_wallet::tx::ARCADE_CLASS_C_MAX)
        .map_err(|e| format!("could not build the record's transaction: {e}"))?;
    let (txid, fee) = crate::arcsvc::send_raw(&tx.raw)?;
    Ok((txid.to_hex(), fee))
}

/// `ledger.announce` on ARCADE: the wallet's messaging key and profile name under its @tag there (a key announcement,
/// what `msg.announce` sends on the home chain), so every arcade reads the profile back from the ARCADE chain.
pub fn announce(node: &Node) -> Result<Value, String> {
    let p = chain()?;
    let (address, tag, _) = sender(node, p)?;
    let tag = tag.ok_or("claim an @tag on the ARCADE chain first (tag.claim with chain arctest)")?;
    let k = crate::msgsvc::keys(node)?;
    let h160 = arcade_ledger::address::p2pkh_hash_of(p, &address).ok_or("bad address")?;
    let a = arcade_msg::public::Announce { key: k.public_bytes(), hash160: Some(h160), name: tag.clone(), tag: Some(tag.clone()), ..Default::default() };
    let (txid, fee) = send(node, &arcade_wallet::tx::any_data(&a.encode()))?;
    let _ = node.app.event("messages", &format!("published your profile for @{tag} on {} (fee {fee})", p.name));
    Ok(json!({"txid": txid, "fee": fee, "tag": tag, "address": address, "chain": p.key}))
}

/// `ledger.profile {address | tag}` on ARCADE: the profile and tag the ARCADE chain's index holds.
pub fn profile(node: &Node, req: &Value) -> Result<Value, String> {
    let p = chain()?;
    let c = p.chain_no;
    let address = match (req["address"].as_str(), req["tag"].as_str()) {
        (Some(a), _) => a.to_string(),
        (None, Some(t)) => arcade_ledger::index::tag_row(&node.chain, c, &t.trim_start_matches('@').to_lowercase()).map_err(|e| e.to_string())?
            .map(|r| r.address).ok_or("no such @tag on the ARCADE chain")?,
        _ => return Err("address or tag required".into()),
    };
    let tag = arcade_ledger::index::tag_of(&node.chain, c, &address).map_err(|e| e.to_string())?;
    let prof = arcade_ledger::index::profile(&node.chain, c, &address).map_err(|e| e.to_string())?;
    Ok(json!({"chain": p.key, "address": address, "tag": tag, "profile": prof,
              "indexed_to": node.chain.indexed_to(c, "ledger.v1").ok().flatten(), "tags": arcade_ledger::index::read_tags(&node.chain, c).map(|t| t.len()).unwrap_or(0)}))
}

fn salt_key(tag: &str) -> Vec<u8> {
    format!("arc_tag_salt:{tag}").into_bytes()
}

/// The salt this wallet committed for `tag` (crate::claims sends its claim with it).
pub fn committed_salt(node: &Node, tag: &str) -> Option<Vec<u8>> {
    node.app.store.read(|r| r.get(arcade_app::META, &salt_key(tag))).ok().flatten()
}

/// `ledger.tag_commit {tag}`: commit ahead to claiming a released @tag (§15 step 8): a commit record naming
/// `commitment(tag, address, salt)` with a fresh salt kept here; the claim (tag.claim) then carries the salt.
pub fn tag_commit(node: &Node, tag: &str) -> Result<Value, String> {
    let p = chain()?;
    let tag = tag.trim().trim_start_matches('@').to_lowercase();
    if let Some(why) = arcade_ledger::tags::tag_problem(&tag) {
        return Err(format!("@{tag}: {why}"));
    }
    let (address, _, _) = sender(node, p)?;
    let mut salt = [0u8; 16];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut salt);
    let c = arcade_ledger::tags::commitment(&tag, &address, &salt);
    node.app.store.write(|w| w.put(arcade_app::META, &salt_key(&tag), &salt)).map_err(|e| e.to_string())?;
    let (txid, fee) = send(node, &arcade_ledger::tags::commit_payload(&c))?;
    Ok(json!({"txid": txid, "fee": fee, "tag": tag, "address": address, "commitment": hex::encode(c)}))
}

/// `ledger.tag_checkin`: keep this wallet's @tag on ARCADE with a check-in (0.1 ARCADE into the pot).
pub fn tag_checkin(node: &Node) -> Result<Value, String> {
    let p = chain()?;
    let (_, tag, sk) = sender(node, p)?;
    let tag = tag.ok_or("this wallet has no @tag on the ARCADE chain")?;
    let x = arcade_ledger::tags::expiry(p.chain_no).ok_or("tags don't expire on this chain")?;
    let pk = arcade_wallet::secp256k1::PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
    let script = arcade_wallet::keys::p2pkh_script_for(&pk);
    let coins = crate::claims::arc_coins(node, p, &script);
    // The fee is the check-in: the rate raised until the transaction pays it.
    let payload = arcade_ledger::tags::checkin_payload(&tag);
    let fee_of = |tx: &arcade_chain::Tx| -> u64 {
        let ins: u64 = tx.inputs.iter().filter_map(|i| coins.iter().find(|c| c.outpoint == i.prevout).map(|c| c.value)).sum();
        ins.saturating_sub(tx.outputs.iter().map(|o| o.value).sum())
    };
    let mut rate = 2 * arcade_arc::params::MIN_FEE_PER_KB;
    let mut built = None;
    for _ in 0..8 {
        let tx = arcade_wallet::tx::build_arcade_with(p, &payload, vec![], &sk, &coins, rate, arcade_arc::params::DUST, arcade_app::now() as u32, arcade_wallet::tx::ARCADE_CLASS_C_MAX)
            .map_err(|e| format!("could not build the check-in: {e}"))?;
        let fee = fee_of(&tx);
        if fee >= x.checkin_fee {
            built = Some(tx);
            break;
        }
        rate = rate.saturating_mul(x.checkin_fee).div_ceil(fee.max(1)) + 1;
    }
    let tx = built.ok_or("could not make a check-in that pays its fee")?;
    let (txid, fee) = crate::arcsvc::send_raw(&tx.raw)?;
    Ok(json!({"txid": txid.to_hex(), "fee": fee, "tag": tag}))
}

/// An @tag's owner on the ARCADE chain, and a warning when a released tag changed hands lately (§15 step 8: sends
/// warn of the owner change).
pub fn tag_owner(node: &Node, tag: &str) -> Option<(String, Option<String>)> {
    let p = chain().ok()?;
    let c = p.chain_no;
    let row = arcade_ledger::index::tag_row(&node.chain, c, tag).ok().flatten()?;
    let warn = arcade_ledger::tags::expiry(c).and_then(|x| {
        let rel = node.chain.store(c).read(|r| r.get(arcade_ledger::engine::TAG_REL, &arcade_store::KeyBuf::new().u16(c).bytes(tag.as_bytes()).done())).ok().flatten()?;
        let rel: Value = serde_json::from_slice(&rel).ok()?;
        let (rel_h, rel_t) = (rel["h"].as_u64()? as u32, rel["t"].as_u64()?);
        let changed = row.block_height >= rel_h && rel["a"].as_str() != Some(row.address.as_str());
        (changed && arcade_app::now() < rel_t + x.warn).then(|| format!(
            "@{tag} changed owner: it was released at block {rel_h} (its owner was inactive) and claimed again at block {} by someone else",
            row.block_height))
    });
    Some((row.address, warn))
}

/// A released @tag's reveal window on ARCADE, while it is open: (released at, window ends at).
pub fn reveal_window(node: &Node, tag: &str) -> Option<(u32, u32)> {
    let p = chain().ok()?;
    let c = p.chain_no;
    let x = arcade_ledger::tags::expiry(c)?;
    let rel = node.chain.store(c).read(|r| r.get(arcade_ledger::engine::TAG_REL, &arcade_store::KeyBuf::new().u16(c).bytes(tag.as_bytes()).done())).ok().flatten()?;
    let rel: Value = serde_json::from_slice(&rel).ok()?;
    let h = rel["h"].as_u64()? as u32;
    let tip = node.chain.tip(c).ok().flatten().map(|t| t.height).unwrap_or(0);
    (tip + 1 < h + x.reveal_blocks).then_some((h, h + x.reveal_blocks))
}

// ---------------- @tag repositories (§15 step 9) ----------------

fn own_full(node: &Node, name: &str) -> Result<(String, Option<String>), String> {
    let p = chain()?;
    let (_, tag, _) = sender(node, p)?;
    let full = arcade_ledger::repos::full_name(name, tag.as_deref()).map_err(|e| format!("{name}: {e}"))?;
    Ok((full, tag))
}

/// `ledger.repo_push {name, manifest}`: point `@owner/name` (or `name` under this wallet's tag) at a git manifest blob.
pub fn repo_push(node: &Node, name: &str, manifest: &str) -> Result<Value, String> {
    let (full, tag) = own_full(node, name)?;
    let tag = tag.ok_or("claim an @tag on the ARCADE chain first")?;
    let m: [u8; 32] = hex::decode(manifest).ok().and_then(|b| b.try_into().ok()).ok_or("manifest is a blob id (64 hex)")?;
    if !crate::arcsvc::holds_blob(&m) {
        return Err("that manifest isn't a blob on the ARCADE chain (yet)".into());
    }
    let (owner, _) = full.split_once('/').unwrap_or_default();
    let c = chain()?.chain_no;
    if owner != tag && !is_maintainer(node, c, &full, &tag) {
        return Err(format!("@{tag} isn't the owner or a maintainer of @{full}"));
    }
    let (txid, fee) = send(node, &arcade_ledger::repos::push_payload(&full, &m))?;
    Ok(json!({"txid": txid, "fee": fee, "repo": format!("@{full}"), "manifest": manifest}))
}

/// `ledger.repo_maintainer {name, tag, add}` (the owner only).
pub fn repo_maintainer(node: &Node, name: &str, tag: &str, add: bool) -> Result<Value, String> {
    let (full, _) = own_full(node, name)?;
    let t = tag.trim_start_matches('@').to_lowercase();
    let (txid, fee) = send(node, &arcade_ledger::repos::maintainer_payload(&full, add, &t))?;
    Ok(json!({"txid": txid, "fee": fee, "repo": format!("@{full}"), "maintainer": t, "add": add}))
}

/// `ledger.repo_fork {from, name}`: `@from` copied under this wallet's tag.
pub fn repo_fork(node: &Node, from: &str, name: &str) -> Result<Value, String> {
    let src = arcade_ledger::repos::full_name(from, None).map_err(|e| format!("{from}: {e}"))?;
    let (full, _) = own_full(node, name)?;
    if repo_row(node, &src).is_none() {
        return Err(format!("no repository @{src} on the ARCADE chain"));
    }
    let (txid, fee) = send(node, &arcade_ledger::repos::fork_payload(&full, &src))?;
    Ok(json!({"txid": txid, "fee": fee, "repo": format!("@{full}"), "from": format!("@{src}")}))
}

fn is_maintainer(node: &Node, c: u16, full: &str, tag: &str) -> bool {
    node.chain.store(c).read(|r| r.get(arcade_ledger::engine::REPO_MAINT, &arcade_store::KeyBuf::new().u16(c).bytes(full.as_bytes()).u8(0).bytes(tag.as_bytes()).done()))
        .ok().flatten().is_some()
}

/// A repository's record on the ARCADE chain (`owner/name`).
pub fn repo_row(node: &Node, full: &str) -> Option<Value> {
    let c = chain().ok()?.chain_no;
    let v = node.chain.store(c).read(|r| r.get(arcade_ledger::engine::REPO, &arcade_store::KeyBuf::new().u16(c).bytes(full.as_bytes()).done())).ok().flatten()?;
    serde_json::from_slice(&v).ok()
}

/// `ledger.repo {name}`: the record, its maintainers and its ref history; `ledger.repos {owner?}`: every repository.
pub fn repo(node: &Node, name: &str) -> Result<Value, String> {
    let full = arcade_ledger::repos::full_name(name, None).map_err(|e| format!("{name}: {e}"))?;
    let c = chain()?.chain_no;
    let row = repo_row(node, &full).ok_or_else(|| format!("no repository @{full} on the ARCADE chain"))?;
    let pre = arcade_store::KeyBuf::new().u16(c).bytes(full.as_bytes()).u8(0).done();
    let maint: Vec<String> = node.chain.store(c).read(|r| r.scan_prefix(arcade_ledger::engine::REPO_MAINT, &pre)).unwrap_or_default()
        .into_iter().map(|(k, _)| String::from_utf8_lossy(&k[pre.len()..]).into_owned()).collect();
    let log: Vec<Value> = node.chain.store(c).read(|r| r.scan_prefix(arcade_ledger::engine::REPO_LOG, &pre)).unwrap_or_default()
        .into_iter().filter_map(|(k, v)| {
            let mut e: Value = serde_json::from_slice(&v).ok()?;
            e["height"] = json!(u32::from_be_bytes(k.get(pre.len()..pre.len() + 4)?.try_into().ok()?));
            Some(e)
        }).collect();
    Ok(json!({"repo": format!("@{full}"), "manifest": row["m"], "fork": row["fork"], "pushes": row["n"], "by": row["by"], "height": row["h"], "maintainers": maint, "log": log}))
}

pub fn repos(node: &Node, owner: Option<&str>) -> Result<Value, String> {
    let c = chain()?.chain_no;
    let mut pre = arcade_store::KeyBuf::new().u16(c);
    if let Some(o) = owner {
        pre = pre.bytes(format!("{}/", o.trim_start_matches('@').to_lowercase()).as_bytes());
    }
    let pre = pre.done();
    let rows = node.chain.store(c).read(|r| r.scan_prefix(arcade_ledger::engine::REPO, &pre)).map_err(|e| e.to_string())?;
    let list: Vec<Value> = rows.into_iter().filter_map(|(k, v)| {
        let v: Value = serde_json::from_slice(&v).ok()?;
        Some(json!({"repo": format!("@{}", String::from_utf8_lossy(&k[2..])), "manifest": v["m"], "fork": v["fork"], "pushes": v["n"]}))
    }).collect();
    Ok(json!({"repos": list}))
}

/// The manifest blob a repository points at now (crate::arcgit: /git/@owner/name).
pub fn repo_manifest(node: &Node, full: &str) -> Option<String> {
    repo_row(node, full)?["m"].as_str().map(String::from)
}

// ---------------- issues, pull requests, comments, reviews (§15 step 10) ----------------

/// A text as a blob (Permanent, JSON): its id. The record that names it can go at once: the ledger keeps the id.
fn body_blob(node: &Node, v: &Value) -> Result<[u8; 32], String> {
    let bytes = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    let r = crate::coinsend::publish_blob(node, &bytes, "application/json", arcade_arc::blob::Class::Permanent, arcade_arc::blob::Encoding::Plain, true)?;
    r["blob"].as_str().and_then(|h| hex::decode(h).ok()).and_then(|b| b.try_into().ok()).ok_or_else(|| "the body's blob has no id".to_string())
}

fn repo_full(name: &str) -> Result<String, String> {
    arcade_ledger::repos::full_name(name, None).map_err(|e| format!("{name}: {e}"))
}

/// `ledger.item_open {repo, kind: issue|pull, title, body, from?, manifest?, head?, extra?}`.
pub fn item_open(node: &Node, req: &Value) -> Result<Value, String> {
    let full = repo_full(req["repo"].as_str().ok_or("repo required")?)?;
    if repo_row(node, &full).is_none() {
        return Err(format!("no repository @{full} on the ARCADE chain"));
    }
    let kind = match req["kind"].as_str().unwrap_or("issue") {
        "issue" => arcade_ledger::repos::KIND_ISSUE,
        "pull" | "pr" | "pull_request" => arcade_ledger::repos::KIND_PULL,
        k => return Err(format!("kind is issue or pull, not {k}")),
    };
    let mut body = json!({"title": req["title"].as_str().ok_or("title required")?, "body": req["body"].as_str().unwrap_or("")});
    for k in ["from", "manifest", "head", "extra"] {
        if !req[k].is_null() {
            body[k] = req[k].clone();
        }
    }
    let b = body_blob(node, &body)?;
    let (txid, fee) = send(node, &arcade_ledger::repos::open_payload(&full, kind, &b))?;
    Ok(json!({"txid": txid, "fee": fee, "repo": format!("@{full}"), "body": hex::encode(b)}))
}

/// `ledger.item_comment {repo, number, body, review?: approve|changes}`.
pub fn item_comment(node: &Node, req: &Value) -> Result<Value, String> {
    let full = repo_full(req["repo"].as_str().ok_or("repo required")?)?;
    let n = req["number"].as_u64().ok_or("number required")? as u32;
    let verdict = match req["review"].as_str() {
        None | Some("") | Some("comment") => 0,
        Some("approve") => 1,
        Some("changes") => 2,
        Some(v) => return Err(format!("review is approve or changes, not {v}")),
    };
    let mut body = json!({"body": req["body"].as_str().unwrap_or("")});
    if !req["extra"].is_null() {
        body["extra"] = req["extra"].clone();
    }
    let b = body_blob(node, &body)?;
    let (txid, fee) = send(node, &arcade_ledger::repos::comment_payload(&full, n, verdict, &b))?;
    Ok(json!({"txid": txid, "fee": fee}))
}

/// `ledger.item_state {repo, number, state: open|closed|merged, manifest?}` (a merge moves the repository to `manifest`).
pub fn item_state(node: &Node, req: &Value) -> Result<Value, String> {
    use arcade_ledger::repos::{STATE_CLOSED, STATE_MERGED, STATE_OPEN};
    let full = repo_full(req["repo"].as_str().ok_or("repo required")?)?;
    let n = req["number"].as_u64().ok_or("number required")? as u32;
    let state = match req["state"].as_str().ok_or("state required")? {
        "open" => STATE_OPEN,
        "closed" | "close" => STATE_CLOSED,
        "merged" | "merge" => STATE_MERGED,
        s => return Err(format!("state is open, closed or merged, not {s}")),
    };
    let m: [u8; 32] = match req["manifest"].as_str() {
        Some(h) => hex::decode(h).ok().and_then(|b| b.try_into().ok()).ok_or("manifest is 64 hex")?,
        None if state == STATE_MERGED => return Err("a merge names the manifest the repository moves to".into()),
        None => [0; 32],
    };
    let (txid, fee) = send(node, &arcade_ledger::repos::state_payload(&full, n, state, &m))?;
    Ok(json!({"txid": txid, "fee": fee}))
}

fn body_of(hash: &str) -> Value {
    hex::decode(hash).ok().and_then(|b| <[u8; 32]>::try_from(b).ok()).and_then(|id| crate::arcblob::view(&id))
        .and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

/// `ledger.items {repo, number?}`: a repository's issues and pull requests with their texts, comments and reviews.
pub fn items(node: &Node, req: &Value) -> Result<Value, String> {
    let full = repo_full(req["repo"].as_str().ok_or("repo required")?)?;
    let c = chain()?.chain_no;
    let pre = arcade_store::KeyBuf::new().u16(c).bytes(full.as_bytes()).u8(0).done();
    let only = req["number"].as_u64().map(|n| n as u32);
    let st = node.chain.store(c);
    let rows = st.read(|r| r.scan_prefix(arcade_ledger::engine::REPO_ITEM, &pre)).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (k, v) in rows {
        let Some(n) = k.get(pre.len()..pre.len() + 4).and_then(|b| b.try_into().ok()).map(u32::from_be_bytes) else { continue };
        if only.is_some_and(|o| o != n) {
            continue;
        }
        let v: Value = serde_json::from_slice(&v).unwrap_or(Value::Null);
        let epre = arcade_store::KeyBuf::new().u16(c).bytes(full.as_bytes()).u8(0).u32(n).done();
        let events: Vec<Value> = st.read(|r| r.scan_prefix(arcade_ledger::engine::REPO_EVENT, &epre)).unwrap_or_default().into_iter().filter_map(|(ek, ev)| {
            let mut e: Value = serde_json::from_slice(&ev).ok()?;
            e["height"] = json!(u32::from_be_bytes(ek.get(epre.len()..epre.len() + 4)?.try_into().ok()?));
            if matches!(e["w"].as_str(), Some("comment" | "review" | "open")) {
                e["text"] = body_of(e["b"].as_str().unwrap_or(""));
            }
            Some(e)
        }).collect();
        let state = ["", "open", "closed", "merged"].get(v["s"].as_u64().unwrap_or(0) as usize).copied().unwrap_or("");
        out.push(json!({"number": n, "kind": if v["k"] == 2 { "pull" } else { "issue" }, "by": v["by"], "state": state,
                        "height": v["h"], "txid": v["txid"], "text": body_of(v["b"].as_str().unwrap_or("")), "events": events}));
    }
    Ok(json!({"repo": format!("@{full}"), "items": out}))
}

/// `ledger.balances {address}`: an address's arcade token balances on ARCADE, each [free, reserved, offered, accepted].
pub fn balances(node: &Node, address: &str) -> Result<Value, String> {
    let c = chain()?.chain_no;
    let pre = arcade_store::KeyBuf::new().u16(c).bytes(address.as_bytes()).u8(0).done();
    let rows = node.chain.store(c).read(|r| r.scan_prefix(arcade_ledger::engine::BALANCE, &pre)).map_err(|e| e.to_string())?;
    let list: Vec<Value> = rows.into_iter().filter_map(|(k, v)| {
        let pid = u32::from_be_bytes(k.get(k.len() - 4..)?.try_into().ok()?);
        let a: Vec<u64> = (0..4).map(|i| v.get(i * 8..i * 8 + 8).map_or(0, |b| u64::from_be_bytes(b.try_into().unwrap()))).collect();
        Some(json!({"id": pid, "amounts": a}))
    }).collect();
    Ok(json!({"address": address, "balances": list}))
}

// ---------------- A:// (profiles, follows, the address book, posts) ----------------

fn scan(node: &Node, c: u16, table: &str, pre: &[u8]) -> Vec<(Vec<u8>, Value)> {
    node.chain.store(c).read(|r| r.scan_prefix(table, pre)).unwrap_or_default().into_iter()
        .filter_map(|(k, v)| Some((k, serde_json::from_slice::<Value>(&v).ok()?))).collect()
}

/// The address an A:// call is about: `address`, else `tag`'s owner, else this wallet's.
fn who(node: &Node, req: &Value) -> Result<String, String> {
    let p = chain()?;
    if let Some(a) = req["address"].as_str().filter(|a| !a.is_empty()) {
        return Ok(a.to_string());
    }
    if let Some(t) = req["tag"].as_str().filter(|t| !t.is_empty()) {
        let t = t.trim_start_matches('@').to_lowercase();
        return arcade_ledger::index::tag_row(&node.chain, p.chain_no, &t).map_err(|e| e.to_string())?.map(|r| r.address).ok_or_else(|| format!("@{t} isn't claimed on the ARCADE chain"));
    }
    Ok(sender(node, p)?.0)
}

fn card(node: &Node, c: u16, address: &str) -> Value {
    let tag = arcade_ledger::index::tag_of(&node.chain, c, address).ok().flatten();
    let prof = node.chain.store(c).read(|r| r.get(arcade_ledger::engine::A_PROFILE, &arcade_store::KeyBuf::new().u16(c).bytes(address.as_bytes()).done())).ok().flatten()
        .and_then(|v| serde_json::from_slice::<Value>(&v).ok());
    let followers = tag.as_ref().map_or(0, |t| scan(node, c, arcade_ledger::engine::A_FOLLOWER, &arcade_store::KeyBuf::new().u16(c).bytes(t.as_bytes()).u8(0).done()).len());
    let following = scan(node, c, arcade_ledger::engine::A_FOLLOW, &arcade_store::KeyBuf::new().u16(c).bytes(address.as_bytes()).u8(0).done()).len();
    json!({"address": address, "tag": tag, "profile": prof.as_ref().map(|p| p["v"].clone()), "height": prof.as_ref().map(|p| p["h"].clone()),
           "versions": prof.as_ref().map_or(json!(0), |p| p["n"].clone()), "followers": followers, "following": following})
}

/// `a.profile {tag | address}`: a profile card as the ARCADE chain's ledger holds it (this wallet's when neither).
pub fn a_profile(node: &Node, req: &Value) -> Result<Value, String> {
    let p = chain()?;
    let c = p.chain_no;
    let a = who(node, req)?;
    let mut v = card(node, c, &a);
    let mine = a == sender(node, p)?.0;
    v["mine"] = json!(mine);
    // No A:// profile yet: the one published on the home chain under the same key (Martin 2026-11-15: keep the name
    // people published; they republish on ARCADE once they have test ARCADE).
    if v["profile"].is_null() {
        if let Some((_, h160)) = arcade_ledger::address::decode_base58check(&a).filter(|(_, h)| h.len() == 20) {
            let home = arcade_ledger::address::base58check(node.params.p2pkh_version, &h160);
            if let Ok(Some(old)) = arcade_ledger::index::profile(&node.chain, node.params.chain_no, &home) {
                let mut pv = serde_json::Map::new();
                for (from, to) in [("name", "n"), ("bio", "b"), ("url", "u")] {
                    if let Some(x) = old[from].as_str().filter(|x| !x.is_empty()) {
                        pv.insert(to.into(), json!(x));
                    }
                }
                v["profile"] = Value::Object(pv);
                v["from_home"] = json!({"chain": node.params.name, "address": home, "height": old["height"], "picture": old["pfp"]});
            }
        }
    }
    if mine {
        // What publishing costs and whether the wallet can pay it (test ARCADE, e.g. from the faucet).
        let (_, _, sk) = sender(node, p)?;
        let pk = arcade_wallet::secp256k1::PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
        let have: u64 = crate::claims::arc_coins(node, p, &arcade_wallet::keys::p2pkh_script_for(&pk)).iter().map(|u| u.value).sum();
        v["spendable"] = json!(have);
        v["can_publish"] = json!(have >= arcade_arc::params::DUST + 100_000);
    }
    Ok(v)
}

/// `a.profile_set {name, bio, url, picture, addresses}`: publish this wallet's profile (one A:// record).
pub fn a_profile_set(node: &Node, req: &Value) -> Result<Value, String> {
    let mut v = serde_json::Map::new();
    for (from, to) in [("name", "n"), ("bio", "b"), ("url", "u"), ("picture", "p")] {
        if let Some(s) = req[from].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            v.insert(to.into(), json!(s));
        }
    }
    if let Some(a) = req["addresses"].as_object().filter(|a| !a.is_empty()) {
        v.insert("a".into(), json!(a.iter().filter(|(_, x)| x.as_str().is_some_and(|s| !s.is_empty())).map(|(k, x)| (k.clone(), x.clone())).collect::<serde_json::Map<_, _>>()));
    }
    let v = Value::Object(v);
    if let Some(p) = arcade_ledger::aproto::profile_problem(&v) {
        return Err(p.into());
    }
    let body = v.to_string();
    if body.len() > arcade_ledger::aproto::MAX_PROFILE {
        return Err(format!("the profile is {} bytes; 1,000 at most", body.len()));
    }
    let (txid, fee) = send(node, &arcade_ledger::aproto::payload(arcade_ledger::aproto::OP_PROFILE, body.as_bytes()))?;
    Ok(json!({"txid": txid, "fee": fee, "bytes": body.len()}))
}

/// `a.follow {tag}` / `a.unfollow {tag}`.
pub fn a_follow(node: &Node, tag: &str, on: bool) -> Result<Value, String> {
    let p = chain()?;
    let t = tag.trim().trim_start_matches('@').to_lowercase();
    if arcade_ledger::index::tag_row(&node.chain, p.chain_no, &t).map_err(|e| e.to_string())?.is_none() {
        return Err(format!("@{t} isn't claimed on the ARCADE chain"));
    }
    let me = sender(node, p)?.0;
    let following = node.chain.store(p.chain_no).read(|r| r.get(arcade_ledger::engine::A_FOLLOW, &arcade_store::KeyBuf::new().u16(p.chain_no).bytes(me.as_bytes()).u8(0).bytes(t.as_bytes()).done())).ok().flatten().is_some();
    if on == following {
        return Err(if on { format!("you already follow @{t}") } else { format!("you don't follow @{t}") });
    }
    let op = if on { arcade_ledger::aproto::OP_FOLLOW } else { arcade_ledger::aproto::OP_UNFOLLOW };
    let (txid, fee) = send(node, &arcade_ledger::aproto::payload(op, t.as_bytes()))?;
    Ok(json!({"txid": txid, "fee": fee, "tag": t, "following": on}))
}

/// `a.follows {tag | address}`: who it follows and who follows it, each with their card.
pub fn a_follows(node: &Node, req: &Value) -> Result<Value, String> {
    let c = chain()?.chain_no;
    let a = who(node, req)?;
    let pre = arcade_store::KeyBuf::new().u16(c).bytes(a.as_bytes()).u8(0).done();
    let following: Vec<Value> = scan(node, c, arcade_ledger::engine::A_FOLLOW, &pre).into_iter().filter_map(|(k, v)| {
        let t = String::from_utf8(k[pre.len()..].to_vec()).ok()?;
        let owner = arcade_ledger::index::tag_row(&node.chain, c, &t).ok().flatten()?.address;
        let mut x = card(node, c, &owner);
        x["since"] = v["h"].clone();
        Some(x)
    }).collect();
    let tag = arcade_ledger::index::tag_of(&node.chain, c, &a).ok().flatten();
    let followers: Vec<Value> = tag.map(|t| {
        let fpre = arcade_store::KeyBuf::new().u16(c).bytes(t.as_bytes()).u8(0).done();
        let mut f: Vec<Value> = scan(node, c, arcade_ledger::engine::A_FOLLOWER, &fpre).into_iter().filter_map(|(k, v)| {
            let addr = String::from_utf8(k[fpre.len()..].to_vec()).ok()?;
            let mut x = card(node, c, &addr);
            x["since"] = v["h"].clone();
            x["you_follow"] = json!(following.iter().any(|g| g["address"] == addr.as_str()));
            Some(x)
        }).collect();
        f.sort_by_key(|x| std::cmp::Reverse(x["since"].as_u64().unwrap_or(0)));
        f
    }).unwrap_or_default();
    Ok(json!({"address": a, "following": following, "followers": followers}))
}

const CONTACT_BOUND: &[u8] = b"arcade A:// contact v1";

/// `a.contacts`: this wallet's address book, opened with its own key.
pub fn a_contacts(node: &Node) -> Result<Value, String> {
    let p = chain()?;
    let me = sender(node, p)?.0;
    let k = crate::msgsvc::keys(node)?;
    let pre = arcade_store::KeyBuf::new().u16(p.chain_no).bytes(me.as_bytes()).u8(0).done();
    let mut out: Vec<Value> = scan(node, p.chain_no, arcade_ledger::engine::A_CONTACT, &pre).into_iter().filter_map(|(key, v)| {
        let sealed = hex::decode(v["c"].as_str()?).ok()?;
        let (_, plain) = arcade_msg::envelope::open(&k.secret, CONTACT_BOUND, &sealed).ok()?;
        let mut e: Value = serde_json::from_slice(&plain).ok()?;
        e["id"] = json!(hex::encode(&key[pre.len()..]));
        e["height"] = v["h"].clone();
        Some(e)
    }).collect();
    out.sort_by(|a, b| a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase()));
    Ok(json!({"contacts": out}))
}

/// `a.contact_save {id?, name, tag?, addresses, note?}`: one entry, sealed to this wallet's own key.
pub fn a_contact_save(node: &Node, req: &Value) -> Result<Value, String> {
    let name = req["name"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("a contact needs a name")?;
    let id: [u8; 8] = match req["id"].as_str() {
        Some(h) => hex::decode(h).ok().and_then(|b| b.try_into().ok()).ok_or("id is 16 hex")?,
        None => {
            let mut b = [0u8; 8];
            rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut b);
            b
        }
    };
    let entry = json!({"name": name, "tag": req["tag"].as_str().map(|t| t.trim().trim_start_matches('@').to_lowercase()).filter(|t| !t.is_empty()),
                       "addresses": req["addresses"].as_object().cloned().unwrap_or_default(), "note": req["note"].as_str().unwrap_or("")});
    let k = crate::msgsvc::keys(node)?;
    let sealed = arcade_msg::envelope::seal(&k.secret, &k.public, CONTACT_BOUND, entry.to_string().as_bytes());
    let mut body = id.to_vec();
    body.extend_from_slice(&sealed);
    if body.len() > arcade_ledger::aproto::MAX_CONTACT {
        return Err("that entry is too long (about 850 bytes of names, addresses and note at most)".into());
    }
    let (txid, fee) = send(node, &arcade_ledger::aproto::payload(arcade_ledger::aproto::OP_CONTACT, &body))?;
    Ok(json!({"txid": txid, "fee": fee, "id": hex::encode(id)}))
}

/// `a.contact_delete {id}`.
pub fn a_contact_delete(node: &Node, id: &str) -> Result<Value, String> {
    let id: [u8; 8] = hex::decode(id).ok().and_then(|b| b.try_into().ok()).ok_or("id is 16 hex")?;
    let (txid, fee) = send(node, &arcade_ledger::aproto::payload(arcade_ledger::aproto::OP_CONTACT_DEL, &id))?;
    Ok(json!({"txid": txid, "fee": fee}))
}

/// `a.post {text}`.
pub fn a_post(node: &Node, text: &str) -> Result<Value, String> {
    let t = text.trim();
    if t.is_empty() || t.len() > arcade_ledger::aproto::MAX_POST {
        return Err("a post is 1 to 1,000 bytes".into());
    }
    let (txid, fee) = send(node, &arcade_ledger::aproto::payload(arcade_ledger::aproto::OP_POST, t.as_bytes()))?;
    Ok(json!({"txid": txid, "fee": fee}))
}

/// `a.posts {tag | address | following: true, limit?}`: posts, newest first (`following`: of everyone this wallet follows).
pub fn a_posts(node: &Node, req: &Value) -> Result<Value, String> {
    let c = chain()?.chain_no;
    let authors: Vec<String> = if req["following"] == true {
        a_follows(node, &json!({}))?["following"].as_array().map(|v| v.iter().filter_map(|x| x["address"].as_str().map(String::from)).collect()).unwrap_or_default()
    } else {
        vec![who(node, req)?]
    };
    let mut posts = Vec::new();
    for a in authors {
        let pre = arcade_store::KeyBuf::new().u16(c).bytes(a.as_bytes()).u8(0).done();
        let tag = arcade_ledger::index::tag_of(&node.chain, c, &a).ok().flatten();
        for (k, v) in scan(node, c, arcade_ledger::engine::A_POST, &pre) {
            let h = k.get(pre.len()..pre.len() + 4).and_then(|b| b.try_into().ok()).map(u32::from_be_bytes).unwrap_or(0);
            posts.push(json!({"address": a, "tag": tag, "text": v["t"], "txid": v["txid"], "height": h}));
        }
    }
    posts.sort_by_key(|p| std::cmp::Reverse(p["height"].as_u64().unwrap_or(0)));
    posts.truncate(req["limit"].as_u64().unwrap_or(100) as usize);
    Ok(json!({"posts": posts}))
}
