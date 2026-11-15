//! A:// on the ARCADE chain (Martin 2026-11-15: "add the d:// protocol to the arcade chain as a:// ... profile
//! publishing, followers and an address book"): small social records every arcade's ledger reads alike.
//!
//! Wire: AnyData payload (version u16, type u16 = 200) whose data is `"A://"` ‖ (1 << 4 | op) ‖ body. The author is
//! the transaction's sender (its signing key: no registrar; the fee joins the pot). Ops:
//! - 1 **profile**: a JSON object of at most [`MAX_PROFILE`] bytes: `n` name (≤ 64), `b` about (≤ 400), `u` link (≤ 200),
//!   `p` picture (a blob id, 64 hex), `a` addresses on other coins (≤ 12, chain → address). The newest wins.
//! - 2 **follow** / 3 **unfollow**: an @tag (claimed on this chain), or a key hash (40 hex) for someone known only by
//!   a name on another chain (kept as `#<hex>`). Unfollows are on chain, so every arcade counts the same followers.
//! - 4 **contact**: an address book entry: id (8) ‖ a sealed box only its author can open. 5 **contact delete**: id (8).
//! - 6 **post**: UTF-8 text of at most [`MAX_POST`] bytes, for its author's followers' feed.

use crate::tags::TYPE_ANY_DATA;
use serde_json::Value;

pub const MARKER: &[u8; 4] = b"A://";
pub const VERSION: u8 = 1;
pub const OP_PROFILE: u8 = 1;
pub const OP_FOLLOW: u8 = 2;
pub const OP_UNFOLLOW: u8 = 3;
pub const OP_CONTACT: u8 = 4;
pub const OP_CONTACT_DEL: u8 = 5;
pub const OP_POST: u8 = 6;
pub const MAX_PROFILE: usize = 1_000;
pub const MAX_POST: usize = 1_000;
pub const MAX_CONTACT: usize = 1_000;

#[derive(Debug, Clone, PartialEq)]
pub enum AOp {
    Profile(Value),
    Follow(String),
    Unfollow(String),
    Contact([u8; 8], Vec<u8>),
    ContactDel([u8; 8]),
    Post(String),
}

fn text_ok(v: &Value, key: &str, most: usize) -> Result<(), &'static str> {
    match &v[key] {
        Value::Null => Ok(()),
        Value::String(s) if s.chars().count() <= most && !s.chars().any(|c| c.is_control() && c != '\n') => Ok(()),
        _ => Err("a profile field is too long or not text"),
    }
}

/// Why a profile isn't acceptable, if it isn't.
pub fn profile_problem(v: &Value) -> Option<&'static str> {
    let o = v.as_object()?;
    if o.keys().any(|k| !["n", "b", "u", "p", "a"].contains(&k.as_str())) {
        return Some("a profile holds n, b, u, p and a only");
    }
    for (k, most) in [("n", 64), ("b", 400), ("u", 200)] {
        if let Err(e) = text_ok(v, k, most) {
            return Some(e);
        }
    }
    if let Some(u) = v["u"].as_str() {
        if !u.is_empty() && !(u.starts_with("https://") || u.starts_with("http://")) {
            return Some("a profile link starts with https://");
        }
    }
    match &v["p"] {
        Value::Null => {}
        Value::String(p) if p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()) => {}
        _ => return Some("a profile picture is a blob id (64 hex)"),
    }
    match &v["a"] {
        Value::Null => {}
        Value::Object(a) if a.len() <= 12 && a.iter().all(|(k, x)| k.len() <= 12 && x.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 100 && s.bytes().all(|b| b.is_ascii_alphanumeric()))) => {}
        _ => return Some("a profile's addresses are up to 12 chain → address pairs"),
    }
    None
}

/// An A:// record. None = not one.
pub fn parse(payload: &[u8]) -> Option<Result<AOp, &'static str>> {
    if payload.len() < 4 || u16::from_be_bytes([payload[2], payload[3]]) != TYPE_ANY_DATA {
        return None;
    }
    let d = &payload[4..];
    if d.len() < 5 || &d[..4] != MARKER {
        return None;
    }
    Some((|| {
        if d[4] >> 4 != VERSION {
            return Err("unknown A:// version");
        }
        let body = &d[5..];
        let tag = |b: &[u8]| -> Result<String, &'static str> {
            let t = std::str::from_utf8(b).map_err(|_| "not utf-8")?.trim_start_matches('@').to_lowercase();
            if t.len() == 40 && t.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Ok(format!("#{t}"));
            }
            if let Some(p) = crate::tags::tag_problem(&t) {
                return Err(p);
            }
            Ok(t)
        };
        let id = |b: &[u8]| -> Result<[u8; 8], &'static str> { b.get(..8).ok_or("truncated")?.try_into().map_err(|_| "truncated") };
        match d[4] & 0x0f {
            OP_PROFILE => {
                if body.len() > MAX_PROFILE {
                    return Err("a profile is 1,000 bytes at most");
                }
                let v: Value = serde_json::from_slice(body).map_err(|_| "a profile is JSON")?;
                if !v.is_object() {
                    return Err("a profile is a JSON object");
                }
                if let Some(p) = profile_problem(&v) {
                    return Err(p);
                }
                Ok(AOp::Profile(v))
            }
            OP_FOLLOW => Ok(AOp::Follow(tag(body)?)),
            OP_UNFOLLOW => Ok(AOp::Unfollow(tag(body)?)),
            OP_CONTACT => {
                if body.len() <= 8 || body.len() > MAX_CONTACT {
                    return Err("a contact is an id and its sealed entry");
                }
                Ok(AOp::Contact(id(body)?, body[8..].to_vec()))
            }
            OP_CONTACT_DEL => Ok(AOp::ContactDel(id(body)?)),
            OP_POST => {
                if body.is_empty() || body.len() > MAX_POST {
                    return Err("a post is 1 to 1,000 bytes");
                }
                Ok(AOp::Post(std::str::from_utf8(body).map_err(|_| "a post is UTF-8 text")?.to_string()))
            }
            _ => Err("unknown A:// op"),
        }
    })())
}

/// The payload of an A:// record (what goes after the "arcd" marker).
pub fn payload(op: u8, body: &[u8]) -> Vec<u8> {
    let mut p = vec![0, 0];
    p.extend_from_slice(&TYPE_ANY_DATA.to_be_bytes());
    p.extend_from_slice(MARKER);
    p.push(VERSION << 4 | op);
    p.extend_from_slice(body);
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_read_back_and_bad_ones_are_named() {
        let prof = json!({"n": "Arcade V1", "b": "keeps chains", "u": "https://app.dogecoinarcade.com", "p": "ab".repeat(32), "a": {"pep": "PJ8Zq", "doge": "DQx4E"}});
        let p = payload(OP_PROFILE, prof.to_string().as_bytes());
        assert_eq!(parse(&p), Some(Ok(AOp::Profile(prof))));
        assert_eq!(parse(&payload(OP_FOLLOW, b"@Shoe")), Some(Ok(AOp::Follow("shoe".into()))));
        assert_eq!(parse(&payload(OP_UNFOLLOW, b"shoe")), Some(Ok(AOp::Unfollow("shoe".into()))));
        assert_eq!(parse(&payload(OP_FOLLOW, "AB".repeat(20).as_bytes())), Some(Ok(AOp::Follow(format!("#{}", "ab".repeat(20))))));
        assert_eq!(parse(&payload(OP_CONTACT, b"12345678sealed")), Some(Ok(AOp::Contact(*b"12345678", b"sealed".to_vec()))));
        assert_eq!(parse(&payload(OP_CONTACT_DEL, b"12345678")), Some(Ok(AOp::ContactDel(*b"12345678"))));
        assert_eq!(parse(&payload(OP_POST, "gm ☀".as_bytes())), Some(Ok(AOp::Post("gm ☀".into()))));
        // Not A:// at all, and A:// records that break the rules.
        assert_eq!(parse(&crate::tags::claim_payload("shoe")), None);
        assert!(matches!(parse(&payload(OP_PROFILE, br#"{"n":"x","evil":1}"#)), Some(Err(_))));
        assert!(matches!(parse(&payload(OP_PROFILE, json!({"u": "javascript:alert(1)"}).to_string().as_bytes())), Some(Err(_))));
        assert!(matches!(parse(&payload(OP_PROFILE, json!({"n": "x".repeat(65)}).to_string().as_bytes())), Some(Err(_))));
        assert!(matches!(parse(&payload(OP_PROFILE, json!({"p": "nope"}).to_string().as_bytes())), Some(Err(_))));
        assert!(matches!(parse(&payload(OP_FOLLOW, b"admin")), Some(Err(_))));
        assert!(matches!(parse(&payload(OP_CONTACT, b"1234")), Some(Err(_))));
        assert!(matches!(parse(&payload(OP_POST, &vec![b'a'; 1001])), Some(Err(_))));
        assert!(matches!(parse(&payload(9, b"x")), Some(Err(_))));
    }
}
