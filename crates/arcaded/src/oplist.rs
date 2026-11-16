//! Operator lists: collections an operator adds to a chain's collections index from a list file,
//! with nothing inscribed (SadFrog 2026-11-15: Anti Doginal Dogs, shared through the operators'
//! own indexes, not on the chain). The file is the ADD repo's OW.json layout
//! ([{id, meta: {name: "X #n", attributes: [{trait_type, value}]}}]), a doggy.market trait file
//! ([{inscriptionId, name: "X #n", attributes: {trait: value}}]), or plain rows
//! ([{id, number, traits: {trait: value}}]).
//!
//! Shared as the rest of the index is: an arcade asking the mesh for a chain's collections gets an
//! operator's lists from the arcade that answers; another operator keeps a copy of one
//! (`collections.list_copy`), read from the mesh and checked against its fingerprint. The
//! fingerprint is the sha256 an on-chain root of the same list carries (collreg::items_digest:
//! every [id, number, [trait value]] in number order, compact JSON), so a list and a root of the
//! same items can be compared.
//!
//! Tables, in the chain's own database (C = chain number):
//!   coll.v1.oc    C|slug             -> JSON {slug, name, description, traits, supply, count, fp, cover, added, from}
//!   coll.v1.oci   C|slug|number u64  -> JSON [id, [trait value]]
//!   coll.v1.ocof  C|iid              -> JSON [slug, number]

use crate::node::Node;
use arcade_chain::params::ChainParams;
use arcade_ledger::ord;
use arcade_store::{KeyBuf, TableDef};
use serde_json::{json, Value};
use std::collections::HashSet;

pub const OC: &str = "coll.v1.oc";
pub const OCI: &str = "coll.v1.oci";
pub const OCOF: &str = "coll.v1.ocof";
pub const TABLES: &[&str] = &[OC, OCI, OCOF];
/// What an operator list's id starts with in explorer.collection(s): `list:<slug>`.
pub const PREFIX: &str = "list:";
const MAX_ITEMS: usize = 100_000;
const MAX_TRAITS: usize = 64;

fn k(c: u16) -> KeyBuf {
    KeyBuf::new().u16(c)
}

fn ensure(node: &Node, c: u16) -> Result<std::sync::Arc<arcade_store::Store>, String> {
    let s = node.chain.wstore(c).map_err(|e| e.to_string())?;
    for t in TABLES {
        s.add_table(TableDef::plain(t)).map_err(|e| e.to_string())?;
    }
    Ok(s)
}

fn slug_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.as_bytes()[0].is_ascii_alphanumeric() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// "Anti Doginal Dogs" -> "anti-doginal-dogs".
pub fn slug_of(name: &str) -> String {
    let mut s = String::new();
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch);
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
    }
    s.trim_end_matches('-').chars().take(64).collect()
}

fn is_item_id(s: &str) -> bool {
    match s.split_once('i') {
        Some((t, n)) => t.len() == 64 && t.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) && !n.is_empty() && n.len() <= 6 && n.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// "Name #12" -> 12 (the original's _number_of: "#", spaces allowed, digits at the end).
fn number_of(name: &str) -> Option<u64> {
    let t = name.trim_end();
    let digits: String = t.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<String>().chars().rev().collect();
    if digits.is_empty() || digits.len() > 9 {
        return None;
    }
    t[..t.len() - digits.len()].trim_end().ends_with('#').then(|| digits.parse().ok()).flatten()
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A list file's traits (first seen first) and its rows [(id, number, [value per trait])] in number order.
pub fn parse(file: &Value) -> Result<(Vec<String>, Vec<(String, u64, Vec<String>)>), String> {
    let rows = file.as_array().ok_or("a list file is a JSON array of its items")?;
    if rows.is_empty() || rows.len() > MAX_ITEMS {
        return Err(format!("a list has 1 to {MAX_ITEMS} items"));
    }
    let mut traits: Vec<String> = Vec::new();
    let mut raw: Vec<(String, u64, Vec<(String, String)>)> = Vec::with_capacity(rows.len());
    for r in rows {
        let (id, number, attrs): (String, Option<u64>, Vec<(String, String)>) = if r["meta"].is_object() {
            // OW.json
            let attrs = r["meta"]["attributes"].as_array().into_iter().flatten().map(|a| (text(&a["trait_type"]), text(&a["value"]))).collect();
            (text(&r["id"]), number_of(r["meta"]["name"].as_str().unwrap_or("")), attrs)
        } else if r["inscriptionId"].is_string() {
            // doggy.market
            let attrs = match &r["attributes"] {
                Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), text(v))).collect(),
                Value::Array(a) => a.iter().map(|a| (text(&a["trait_type"]), text(&a["value"]))).collect(),
                _ => Vec::new(),
            };
            (text(&r["inscriptionId"]), number_of(r["name"].as_str().unwrap_or("")), attrs)
        } else {
            let attrs = r["traits"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), text(v))).collect()).unwrap_or_default();
            (text(&r["id"]), r["number"].as_u64(), attrs)
        };
        let id = id.trim().to_ascii_lowercase();
        if !is_item_id(&id) {
            return Err(format!("{id:?} is not an inscription id (<txid>i<n>)"));
        }
        let number = number.filter(|n| *n >= 1).ok_or_else(|| format!("{id}: no number (a name ending in \"#12\", or number)"))?;
        for (t, _) in &attrs {
            if !t.is_empty() && !traits.contains(t) {
                traits.push(t.clone());
            }
        }
        raw.push((id, number, attrs));
    }
    if traits.len() > MAX_TRAITS {
        return Err(format!("at most {MAX_TRAITS} traits"));
    }
    let mut out: Vec<(String, u64, Vec<String>)> = raw
        .into_iter()
        .map(|(id, n, attrs)| {
            let vals = traits.iter().map(|t| attrs.iter().find(|(k, _)| k == t).map(|(_, v)| v.clone()).unwrap_or_default()).collect();
            (id, n, vals)
        })
        .collect();
    out.sort_by_key(|r| r.1);
    let (mut ids, mut nums) = (HashSet::new(), HashSet::new());
    for (id, n, _) in &out {
        if !ids.insert(id.as_str()) {
            return Err(format!("{id} is listed twice"));
        }
        if !nums.insert(*n) {
            return Err(format!("two items are numbered {n}"));
        }
    }
    Ok((traits, out))
}

/// The list's fingerprint: the sha256 an on-chain root of the same items carries.
pub fn fingerprint(rows: &[(String, u64, Vec<String>)]) -> String {
    let r: Vec<(Value, Value, Vec<String>)> = rows.iter().map(|(id, n, t)| (json!(id), json!(n), t.clone())).collect();
    arcade_ledger::collreg::items_digest(&r)
}

// ---------------- the store ----------------

pub fn line(node: &Node, c: u16, slug: &str) -> Option<Value> {
    node.chain.store(c).read(|r| r.get(OC, &k(c).var(slug.as_bytes()).done())).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok())
}

/// Every operator list of chain `c`.
pub fn lines(node: &Node, c: u16) -> Vec<Value> {
    node.chain.store(c).read(|r| r.scan_prefix(OC, &k(c).done())).unwrap_or_default().into_iter().filter_map(|(_, v)| serde_json::from_slice(&v).ok()).collect()
}

/// A page of a list's items by number: (number, [id, [trait value]]).
pub fn items(node: &Node, c: u16, slug: &str, offset: u64, n: usize) -> Vec<(u64, Value)> {
    let pre = k(c).var(slug.as_bytes()).done();
    let mut out = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    let mut skipped = 0;
    loop {
        let rows = node.chain.store(c).read(|r| r.scan_after(OCI, &pre, after.as_deref(), 5_000)).unwrap_or_default();
        if rows.is_empty() {
            return out;
        }
        after = rows.last().map(|x| x.0.clone());
        for (key, v) in rows {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if out.len() >= n {
                return out;
            }
            let num = arcade_store::key::be_u64(&key, key.len() - 8);
            out.push((num, serde_json::from_slice(&v).unwrap_or(Value::Null)));
        }
    }
}

/// The operator list a piece is in here: (slug, number).
pub fn of(node: &Node, c: u16, iid: &[u8; 36]) -> Option<(String, u64)> {
    let v: Value = serde_json::from_slice(&node.chain.store(c).read(|r| r.get(OCOF, &k(c).bytes(iid).done())).ok()??).ok()?;
    Some((v[0].as_str()?.to_string(), v[1].as_u64()?))
}

fn drop_items(node: &Node, c: u16, slug: &str) -> Result<(), String> {
    let s = ensure(node, c)?;
    let pre = k(c).var(slug.as_bytes()).done();
    loop {
        let old = s.read(|r| r.scan_prefix_limit(OCI, &pre, 20_000)).map_err(|e| e.to_string())?;
        if old.is_empty() {
            return Ok(());
        }
        s.write(|w| {
            for (key, v) in &old {
                let a: Value = serde_json::from_slice(v).unwrap_or(Value::Null);
                if let Some(x) = ord::parse_iid(a[0].as_str().unwrap_or("")) {
                    let mine = w.get(OCOF, &k(c).bytes(&x).done())?.and_then(|v| serde_json::from_slice::<Value>(&v).ok()).is_some_and(|v| v[0] == slug);
                    if mine {
                        w.del(OCOF, &k(c).bytes(&x).done())?;
                    }
                }
                w.del(OCI, key)?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    }
}

/// Keep a list (replacing one of the same slug): its line, then its items.
fn keep(node: &Node, p: &ChainParams, mut head: Value, traits: &[String], rows: &[(String, u64, Vec<String>)]) -> Result<Value, String> {
    let c = p.chain_no;
    let slug = head["slug"].as_str().unwrap_or("").to_string();
    let fp = fingerprint(rows);
    if let Some(want) = head["fp"].as_str().filter(|f| !f.is_empty()) {
        if want != fp {
            return Err(format!("the items are not the list's (fingerprint {fp}, the list says {want})"));
        }
    }
    let s = ensure(node, c)?;
    // Refused before anything is dropped: a replace that fails keeps the list it would replace.
    let taken: Vec<String> = rows.iter().filter_map(|(id, _, _)| of(node, c, &ord::parse_iid(id)?).filter(|(s, _)| *s != slug).map(|(s, _)| s)).collect();
    if let Some(other) = taken.first() {
        return Err(format!("{} of its items are in the list {other} here already: remove that one first", taken.len()));
    }
    drop_items(node, c, &slug)?;
    for chunk in rows.chunks(5_000) {
        s.write(|w| {
            for (id, n, vals) in chunk {
                let Some(x) = ord::parse_iid(id) else { continue };
                w.put(OCI, &k(c).var(slug.as_bytes()).u64(*n).done(), json!([id, vals]).to_string().as_bytes())?;
                w.put(OCOF, &k(c).bytes(&x).done(), json!([slug, n]).to_string().as_bytes())?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    }
    let supply = head["supply"].as_u64().unwrap_or(0).max(rows.last().map(|r| r.1).unwrap_or(0));
    head["traits"] = json!(traits);
    head["supply"] = json!(supply);
    head["count"] = json!(rows.len());
    head["fp"] = json!(fp);
    if head["cover"].as_str().unwrap_or("").is_empty() {
        head["cover"] = json!(rows.first().map(|r| r.0.clone()));
    }
    head["added"] = json!(arcade_app::now());
    s.write(|w| w.put(OC, &k(c).var(slug.as_bytes()).done(), head.to_string().as_bytes())).map_err(|e| e.to_string())?;
    Ok(head)
}

fn need_index(node: &Node, p: &ChainParams) -> Result<(), String> {
    if crate::collsvc::chains(node).iter().any(|x| x.chain_no == p.chain_no) {
        Ok(())
    } else {
        Err(format!("turn on the collections index for {} in Chains & Coins first", p.name))
    }
}

/// `collections.list_add {chain, name, file, slug?, description?, supply?}`: add (or replace) an
/// operator list from a list file (a JSON array, or its text).
pub fn add(node: &Node, p: &ChainParams, req: &Value) -> Result<Value, String> {
    need_index(node, p)?;
    let name = req["name"].as_str().unwrap_or("").trim().to_string();
    if name.is_empty() || name.chars().count() > 80 {
        return Err("a list has a name, up to 80 characters".into());
    }
    let slug = req["slug"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| slug_of(&name));
    if !slug_ok(&slug) {
        return Err("a slug is lower-case letters, digits and hyphens, up to 64".into());
    }
    let file: Value = match &req["file"] {
        Value::String(t) => serde_json::from_str(t).map_err(|e| format!("the list file is not JSON: {e}"))?,
        v => v.clone(),
    };
    let (traits, rows) = parse(&file)?;
    let head = json!({"slug": slug, "name": name, "description": req["description"].as_str().unwrap_or("").chars().take(2000).collect::<String>(),
                      "supply": req["supply"].as_u64().unwrap_or(0), "cover": req["cover"].as_str().unwrap_or("").trim().to_ascii_lowercase(), "from": "file"});
    let v = keep(node, p, head, &traits, &rows)?;
    let _ = node.app.event("collections", &format!("added the list {name} on {} ({} items)", p.name, rows.len()));
    Ok(v)
}

/// `collections.list_remove {chain, slug}`.
pub fn remove(node: &Node, p: &ChainParams, slug: &str) -> Result<Value, String> {
    let c = p.chain_no;
    line(node, c, slug).ok_or_else(|| format!("no list {slug} on {} here", p.name))?;
    drop_items(node, c, slug)?;
    ensure(node, c)?.write(|w| w.del(OC, &k(c).var(slug.as_bytes()).done())).map_err(|e| e.to_string())?;
    Ok(json!({"removed": slug, "chain": p.key}))
}

/// `collections.list_copy {chain, slug}`: keep here a list another arcade keeps, read from the mesh
/// a page at a time and kept only if its items match the list's fingerprint.
pub fn copy(node: &Node, p: &ChainParams, slug: &str) -> Result<Value, String> {
    need_index(node, p)?;
    if line(node, p.chain_no, slug).is_some() {
        return Err(format!("the list {slug} is kept here already"));
    }
    let id = format!("{PREFIX}{slug}");
    let page = |off: u64| crate::explorer::handle(node, "explorer.collection", &json!({"chain": p.key, "id": id, "offset": off, "n": 96}));
    let first = page(0)?;
    if first["kind"] != "list" || first["fp"].as_str().unwrap_or("").is_empty() {
        return Err(format!("no arcade answered for the list {slug} on {}", p.name));
    }
    let traits: Vec<String> = first["traits"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)).collect();
    let total = first["total"].as_u64().unwrap_or(0) as usize;
    if total == 0 || total > MAX_ITEMS {
        return Err("that list has no items, or too many".into());
    }
    let mut rows: Vec<(String, u64, Vec<String>)> = Vec::with_capacity(total);
    let mut got = first;
    loop {
        for it in got["items"].as_array().into_iter().flatten() {
            let vals: Vec<String> = it["values"].as_array().into_iter().flatten().map(text).collect();
            let id = it["id"].as_str().unwrap_or("").to_ascii_lowercase();
            if !is_item_id(&id) || vals.len() != traits.len() {
                return Err("an arcade answered an item without its id or its trait values".into());
            }
            rows.push((id, it["number"].as_u64().unwrap_or(0), vals));
        }
        if rows.len() >= total || got["items"].as_array().map_or(true, |a| a.is_empty()) {
            break;
        }
        got = page(rows.len() as u64)?;
    }
    rows.sort_by_key(|r| r.1);
    rows.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    let head = json!({"slug": slug, "name": got["name"], "description": got["description"], "supply": got["supply"], "cover": got["cover"],
                      "fp": got["fp"], "from": "mesh"});
    let v = keep(node, p, head, &traits, &rows)?;
    let _ = node.app.event("collections", &format!("kept a copy of the list {} on {} ({} items, fingerprint checked)", got["name"].as_str().unwrap_or(slug), p.name, rows.len()));
    Ok(v)
}

// ---------------- explorer rows ----------------

/// A list's row in explorer.collections.
pub fn row(v: &Value, chain: &str) -> Value {
    let (supply, count) = (v["supply"].as_u64().unwrap_or(0), v["count"].as_u64().unwrap_or(0));
    json!({"kind": "list", "id": format!("{PREFIX}{}", v["slug"].as_str().unwrap_or("")), "slug": v["slug"], "name": v["name"], "chain": chain,
           "supply": supply, "count": count, "missing": supply.saturating_sub(count), "cover": v["cover"], "description": v["description"],
           "traits": v["traits"], "fp": v["fp"], "from": v["from"], "added": v["added"], "publisher": null, "height": null, "number": null,
           "status": "active", "reason": ""})
}

/// explorer.collection for `list:<slug>`: the row and a page of items {number, id, inscription, traits, values}.
pub fn get(node: &Node, p: &ChainParams, slug: &str, off: u64, n: usize) -> Result<Value, String> {
    let c = p.chain_no;
    let l = line(node, c, slug).ok_or_else(|| format!("no collection {PREFIX}{slug} on {}", p.name))?;
    let traits: Vec<String> = l["traits"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)).collect();
    let items: Vec<Value> = items(node, c, slug, off, n)
        .into_iter()
        .map(|(num, it)| {
            let id = it[0].as_str().unwrap_or("").to_string();
            let vals = it[1].clone();
            // What is shown: a value that says nothing ("", "None") is left out.
            let t: serde_json::Map<String, Value> = traits
                .iter()
                .zip(vals.as_array().into_iter().flatten())
                .filter(|(_, v)| v.as_str().is_some_and(|s| !s.is_empty() && s != "None"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let insc = ord::parse_iid(&id).and_then(|b| ord::number_of(&node.chain, c, &b));
            json!({"number": num, "id": id, "inscription": insc, "traits": t, "values": vals})
        })
        .collect();
    let mut v = row(&l, p.key);
    v["total"] = l["count"].clone();
    v["offset"] = json!(off);
    v["items"] = json!(items);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1e178b1ce6b9680a5f224a2984e008729e26401cf7b5ce8f41f248126c0edf81i0";
    const B: &str = "dbc3598377ef3bab4c107e9b5f1565e2e51af59fa8ed98d17f9d4b14e1c97f29i0";

    #[test]
    fn three_layouts_one_list() {
        let ow = json!([
            {"id": B, "meta": {"name": "Anti Doginal Dog #2", "attributes": [{"trait_type": "Background", "value": "Yellow"}, {"trait_type": "Mouth", "value": "None"}]}},
            {"id": A, "meta": {"name": "Anti Doginal Dog #1", "attributes": [{"trait_type": "Background", "value": "Blue"}, {"trait_type": "Mouth", "value": "None"}]}}
        ]);
        let dm = json!([
            {"inscriptionId": A, "name": "Anti Doginal Dog #1", "attributes": {"Background": "Blue", "Mouth": "None"}},
            {"inscriptionId": B, "name": "Anti Doginal Dog # 2", "attributes": {"Background": "Yellow", "Mouth": "None"}}
        ]);
        let plain = json!([{"id": A.to_uppercase(), "number": 1, "traits": {"Background": "Blue", "Mouth": "None"}},
                           {"id": B, "number": 2, "traits": {"Background": "Yellow", "Mouth": "None"}}]);
        let (t, a) = parse(&ow).unwrap();
        assert_eq!(t, ["Background", "Mouth"]);
        assert_eq!(a[0], (A.to_string(), 1, vec!["Blue".to_string(), "None".to_string()]));
        assert_eq!(parse(&dm).unwrap(), (t.clone(), a.clone()));
        assert_eq!(parse(&plain).unwrap(), (t, a.clone()));
        // The original's builder (collection_json.py) gives the same sha256 for the same items:
        // python3 -c 'import json,hashlib;print(hashlib.sha256(json.dumps([[A,1,["Blue","None"]],[B,2,["Yellow","None"]]],separators=(",",":")).encode()).hexdigest())'
        let want = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(
            format!(r#"[["{A}",1,["Blue","None"]],["{B}",2,["Yellow","None"]]]"#).as_bytes(),
        ));
        assert_eq!(fingerprint(&a), want);
    }

    #[test]
    fn kept_listed_found_and_removed() {
        let (_one, t) = crate::http::tests::node();
        let node: &Node = &t.node;
        let p = node.params;
        let c = p.chain_no;
        let file = json!([{"id": B, "number": 2, "traits": {"Background": "Yellow", "Mouth": "None"}},
                          {"id": A, "number": 1, "traits": {"Background": "Blue", "Mouth": "None"}}]);
        let (traits, rows) = parse(&file).unwrap();
        let head = json!({"slug": "add", "name": "Anti Doginal Dogs", "description": "", "supply": 10000, "cover": "", "from": "file"});
        let v = keep(node, p, head.clone(), &traits, &rows).unwrap();
        assert_eq!((v["count"].as_u64(), v["supply"].as_u64(), v["cover"].as_str()), (Some(2), Some(10000), Some(A)));
        assert_eq!(v["fp"], fingerprint(&rows));
        assert_eq!(lines(node, c).len(), 1);
        let g = get(node, p, "add", 0, 48).unwrap();
        assert_eq!((g["kind"].as_str(), g["id"].as_str(), g["missing"].as_u64()), (Some("list"), Some("list:add"), Some(9998)));
        assert_eq!((g["items"][0]["number"].as_u64(), g["items"][1]["id"].as_str()), (Some(1), Some(B)));
        assert!(g["items"][0]["traits"].get("Mouth").is_none(), "None is not shown");
        assert_eq!(g["items"][0]["values"], json!(["Blue", "None"]), "but is kept, for the fingerprint");
        assert_eq!(get(node, p, "add", 1, 48).unwrap()["items"].as_array().unwrap().len(), 1);
        assert_eq!(of(node, c, &ord::parse_iid(A).unwrap()), Some(("add".to_string(), 1)));
        // Kept again (replaced): still two items, still one list.
        keep(node, p, head.clone(), &traits, &rows).unwrap();
        assert_eq!(items(node, c, "add", 0, 96).len(), 2);
        // Another list naming the same items, or items that don't match a fingerprint: refused.
        let mut other = head.clone();
        other["slug"] = json!("other");
        assert!(keep(node, p, other.clone(), &traits, &rows).unwrap_err().contains("in the list add"));
        // Replacing a list with items another list holds is refused, and keeps the list as it was.
        let mut small = head.clone();
        small["slug"] = json!("small");
        keep(node, p, small.clone(), &traits, &[("aa".repeat(32) + "i0", 9, vec!["Red".into(), "None".into()])]).unwrap();
        assert!(keep(node, p, small, &traits, &rows).unwrap_err().contains("in the list add"));
        assert_eq!(items(node, c, "small", 0, 96).len(), 1, "small keeps its item");
        remove(node, p, "small").unwrap();
        other["fp"] = json!("00".repeat(32));
        assert!(keep(node, p, other, &traits, &rows).unwrap_err().contains("not the list's"));
        remove(node, p, "add").unwrap();
        assert!(lines(node, c).is_empty() && items(node, c, "add", 0, 96).is_empty());
        assert!(of(node, c, &ord::parse_iid(A).unwrap()).is_none());
    }

    #[test]
    fn bad_lists_are_refused() {
        assert!(parse(&json!([])).is_err());
        assert!(parse(&json!({"id": A})).is_err());
        assert!(parse(&json!([{"id": "nope", "number": 1}])).unwrap_err().contains("not an inscription id"));
        assert!(parse(&json!([{"id": A, "meta": {"name": "no number"}}])).unwrap_err().contains("no number"));
        assert!(parse(&json!([{"id": A, "number": 1}, {"id": A, "number": 2}])).unwrap_err().contains("twice"));
        assert!(parse(&json!([{"id": A, "number": 1}, {"id": B, "number": 1}])).unwrap_err().contains("numbered 1"));
        assert_eq!(slug_of("  Anti Doginal Dogs! "), "anti-doginal-dogs");
        assert!(slug_ok("anti-doginal-dogs") && !slug_ok("Anti") && !slug_ok("-a") && !slug_ok(""));
        assert_eq!(number_of("Dog #12"), Some(12));
        assert_eq!(number_of("Dog 12"), None);
    }
}
