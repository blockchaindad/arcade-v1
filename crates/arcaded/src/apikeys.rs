//! API keys for websites and other programs using the web API (/api/v1).
//! A key is "ak_<id>_<secret>"; only sha256(secret) is kept (arcade.mdbx META "apikey:<id>"),
//! so a key is shown once when made. Each key has permissions (commands::PERMS) and, for tips,
//! a daily limit in satoshis.

use crate::commands::{self, Perm};
use crate::node::Node;
use serde_json::{json, Value};
use sha2::Digest;

fn key_of(id: &str) -> Vec<u8> {
    format!("apikey:{id}").into_bytes()
}

fn today() -> u64 {
    arcade_app::now() / 86400
}

pub fn list(node: &Node) -> Value {
    let rows = node.app.store.read(|r| r.scan_prefix(arcade_app::META, b"apikey:")).unwrap_or_default();
    json!(rows
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_slice::<Value>(&v).ok())
        .map(|mut v| {
            v.as_object_mut().map(|o| o.remove("hash"));
            v
        })
        .collect::<Vec<_>>())
}

pub fn create(node: &Node, name: &str, perms: &[String], tip_limit: u64) -> Result<Value, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 60 {
        return Err("give the key a name (1-60 characters), e.g. the website it is for".into());
    }
    let mut ps = Vec::new();
    for p in perms {
        let perm = Perm::from_name(p).filter(|p| commands::PERMS.contains(p)).ok_or_else(|| format!("unknown permission {p}; one of {}", commands::PERMS.iter().map(|p| p.name()).collect::<Vec<_>>().join(", ")))?;
        if !ps.contains(&perm.name()) {
            ps.push(perm.name());
        }
    }
    let (id, secret) = (arcade_mesh::random::hex(4), arcade_mesh::random::hex(16));
    let rec = json!({"id": id, "name": name, "perms": ps, "tip_limit": tip_limit, "created": arcade_app::now(), "last_used": Value::Null,
                     "spent": {"day": today(), "sats": 0}, "hash": hex::encode(sha2::Sha256::digest(secret.as_bytes()))});
    node.app.store.write(|w| w.put(arcade_app::META, &key_of(&id), rec.to_string().as_bytes())).map_err(|e| e.to_string())?;
    let _ = node.app.event("api", &format!("API key \"{name}\" made ({})", ps.join(", ")));
    Ok(json!({"id": id, "key": format!("ak_{id}_{secret}"), "perms": ps, "tip_limit": tip_limit, "note": "Copy the key now: it isn't shown again."}))
}

pub fn revoke(node: &Node, id: &str) -> Result<Value, String> {
    let gone = node.app.store.write(|w| w.del(arcade_app::META, &key_of(id))).map_err(|e| e.to_string())?;
    if !gone {
        return Err(format!("no key {id}"));
    }
    let _ = node.app.event("api", &format!("API key {id} revoked"));
    Ok(json!({"revoked": id}))
}

/// Whether `key` is a key made here and not revoked (before a large body is read for it).
pub fn known(node: &Node, key: &str) -> bool {
    let Some((id, secret)) = key.strip_prefix("ak_").and_then(|k| k.split_once('_')) else { return false };
    let rec: Option<Value> = node.app.store.read(|r| r.get(arcade_app::META, &key_of(id))).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok());
    rec.is_some_and(|rec| rec["hash"] == hex::encode(sha2::Sha256::digest(secret.as_bytes())).as_str())
}

/// May this request run? `key` is the bearer token, if any. Records use (and tip spending).
const ONE_DOGE: u64 = 100_000_000;

/// An amount in DOGE (a string or a number, as `dogefeed.act` takes it) in koinu.
fn doge_koinu(v: &Value) -> Option<u64> {
    let x = v.as_str().map(String::from).or_else(|| v.as_f64().map(|f| f.to_string()))?;
    let (whole, frac) = x.trim().split_once('.').unwrap_or((x.trim(), ""));
    let frac: String = frac.chars().chain(std::iter::repeat('0')).take(8).collect();
    whole.parse::<u64>().ok()?.checked_mul(ONE_DOGE)?.checked_add(frac.parse::<u64>().ok()?)
}

pub fn allow(node: &Node, key: Option<&str>, req: &Value) -> Result<(), (u16, String)> {
    let cmd = req["cmd"].as_str().unwrap_or("");
    let c = commands::find(cmd).ok_or((404, format!("unknown command {cmd}")))?;
    let mut need = c.perm;
    // What a write spends from the tip allowance, in the smallest unit (None: it spends nothing).
    let mut spends: Option<u64> = None;
    if cmd == "feed.act" && req["kind"] == "tip" {
        need = Perm::Tip;
        spends = Some(req["amount"].as_u64().unwrap_or(0));
    }
    // D:// writes spend this arcade's mainnet DOGE (a tip or reveal its amount, any other write about 1 DOGE in
    // all): sent ones need "tip" and count against the key's daily limit, like feed tips. Without send they only
    // say what they would cost.
    if (cmd == "dogefeed.act" || cmd == "dogefeed.post") && req["send"] == true {
        need = Perm::Tip;
        let tip = matches!(req["op"].as_str(), Some("tip" | "reveal")).then(|| doge_koinu(&req["amount"])).flatten();
        spends = Some(tip.unwrap_or(ONE_DOGE));
    }
    // A file named by path is read from this computer's disk: only its own programs may name one (the socket or
    // the cookie RPC). Over a key, send the bytes as hex.
    if cmd.starts_with("arc.blob_publish") && (req["path"].is_string() || req["files"].as_array().is_some_and(|f| f.iter().any(|x| x["path"].is_string()))) {
        return Err((403, format!("{cmd} with a file path is only available on this computer's own socket or the cookie RPC; send the bytes as hex")));
    }
    if cmd == "feed.settings" && req["set"].is_object() {
        need = Perm::Post;
    }
    match need {
        Perm::Public => return Ok(()),
        Perm::Local => return Err((403, format!("{cmd} is only available on this computer's own socket or the cookie RPC"))),
        _ => {}
    }
    let key = key.ok_or((401, format!("{cmd} needs an API key with \"{}\" (Authorization: Bearer ak_...)", need.name())))?;
    let (id, secret) = key.strip_prefix("ak_").and_then(|k| k.split_once('_')).ok_or((401, "malformed API key".to_string()))?;
    let mut rec: Value = node
        .app
        .store
        .read(|r| r.get(arcade_app::META, &key_of(id)))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .ok_or((401, "unknown or revoked API key".to_string()))?;
    if rec["hash"] != hex::encode(sha2::Sha256::digest(secret.as_bytes())).as_str() {
        return Err((401, "unknown or revoked API key".into()));
    }
    if !rec["perms"].as_array().is_some_and(|p| p.iter().any(|x| x == need.name())) {
        return Err((403, format!("this key can't {cmd}: it needs \"{}\"", need.name())));
    }
    if let Some(amount) = spends {
        if rec["spent"]["day"].as_u64() != Some(today()) {
            rec["spent"] = json!({"day": today(), "sats": 0});
        }
        let spent = rec["spent"]["sats"].as_u64().unwrap_or(0);
        let limit = rec["tip_limit"].as_u64().unwrap_or(0);
        let after = spent.checked_add(amount).filter(|t| *t <= limit);
        let Some(after) = after else {
            return Err((403, format!("this key's tip limit is {limit} satoshis a day; {spent} used today")));
        };
        rec["spent"]["sats"] = json!(after);
    }
    rec["last_used"] = json!(arcade_app::now());
    let _ = node.app.store.write(|w| w.put(arcade_app::META, &key_of(id), rec.to_string().as_bytes()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tip_that_would_wrap_the_days_total_is_refused() {
        let (_one, t) = crate::http::tests::node();
        let k = create(&t.node, "site", &["tip".into()], 1000).unwrap();
        let key = k["key"].as_str().unwrap();
        let tip = |amount: u64| allow(&t.node, Some(key), &json!({"cmd": "feed.act", "kind": "tip", "amount": amount}));
        assert!(tip(600).is_ok());
        // 600 + this wraps to 499 in a 64-bit sum: under the limit if it wrapped.
        assert_eq!(tip(u64::MAX - 100).unwrap_err().0, 403);
        assert_eq!(tip(401).unwrap_err().0, 403, "the day's total is still 600");
        assert!(tip(400).is_ok());
        assert!(known(&t.node, key));
        assert!(!known(&t.node, &format!("{key}x")));
        assert!(!known(&t.node, "ak_nope_nope"));
    }

    /// D:// writes that send spend mainnet DOGE: a "wallet" key alone can't send them, and a "tip" key's sends
    /// count against its daily limit (a tip by its amount, any other write as 1 DOGE).
    #[test]
    fn dogefeed_sends_need_tip_and_count_against_the_limit() {
        let (_one, t) = crate::http::tests::node();
        let w = create(&t.node, "w", &["wallet".into()], 0).unwrap();
        let w = w["key"].as_str().unwrap();
        let act = |key: &str, v: Value| allow(&t.node, Some(key), &v);
        assert!(act(w, json!({"cmd": "dogefeed.act", "op": "like", "target": "x"})).is_ok(), "without send it only says the cost");
        assert_eq!(act(w, json!({"cmd": "dogefeed.act", "op": "like", "target": "x", "send": true})).unwrap_err().0, 403);
        assert_eq!(act(w, json!({"cmd": "dogefeed.act", "op": "tip", "target": "x", "amount": "5", "send": true})).unwrap_err().0, 403);
        let k = create(&t.node, "t", &["tip".into(), "wallet".into()], 3 * ONE_DOGE).unwrap();
        let k = k["key"].as_str().unwrap();
        assert!(act(k, json!({"cmd": "dogefeed.act", "op": "tip", "target": "x", "amount": "1.5", "send": true})).is_ok());
        assert!(act(k, json!({"cmd": "dogefeed.act", "op": "like", "target": "x", "send": true})).is_ok(), "1.5 + 1 of 3");
        assert_eq!(act(k, json!({"cmd": "dogefeed.post", "text": "hi", "send": true})).unwrap_err().0, 403, "another 1 DOGE is over 3");
        assert!(act(k, json!({"cmd": "dogefeed.act", "op": "tip", "target": "x", "amount": 0.5, "send": true})).is_ok(), "exactly 3");
        assert_eq!(doge_koinu(&json!("0.00000001")), Some(1));
        assert_eq!(doge_koinu(&json!("x")), None);
    }

    /// A file path is read from this computer's disk: never over an API key, whatever its permissions.
    #[test]
    fn a_key_cannot_name_a_file_to_publish() {
        let (_one, t) = crate::http::tests::node();
        let k = create(&t.node, "w", &["wallet".into()], 0).unwrap();
        let k = k["key"].as_str().unwrap();
        let r = |v: Value| allow(&t.node, Some(k), &v);
        assert_eq!(r(json!({"cmd": "arc.blob_publish", "path": "/home/x/.local/share/arcade/secret.key", "type": "text/plain"})).unwrap_err().0, 403);
        assert!(r(json!({"cmd": "arc.blob_publish_many", "files": [{"path": "/etc/passwd"}]})).is_err());
        assert!(r(json!({"cmd": "arc.blob_publish", "hex": "6869", "type": "text/plain"})).is_ok());
    }
}
