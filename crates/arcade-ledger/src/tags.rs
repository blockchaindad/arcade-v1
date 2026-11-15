//! @tag claims and transfers (tags.py; state.py:949-958, 1374-1412).
//!
//! Wire: AnyData payload (version u16 BE, type u16 BE = 200) whose data is
//! "INSC" + [1, kind, len] + utf-8 tag; kind 3 = claim, 4 = transfer.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const TYPE_ANY_DATA: u16 = 200;
pub const KIND_CLAIM: u8 = 3;
pub const KIND_TRANSFER: u8 = 4;
pub const MIN_LENGTH: usize = 2;
pub const MAX_LENGTH: usize = 24;
pub const RESERVED: &[&str] = &[
    "admin", "administrator", "arcade", "dogecoinarcade", "support", "help", "official", "system",
    "root", "moderator", "mod", "staff", "team", "pepecoin", "dogecoin", "null", "none",
    "anonymous", "me", "you",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagOp {
    Claim(String),
    Transfer(String),
}

/// Why a tag is not acceptable, or None if it is.
pub fn tag_problem(tag: &str) -> Option<&'static str> {
    if tag.len() < MIN_LENGTH {
        return Some("too short");
    }
    if tag.len() > MAX_LENGTH {
        return Some("too long");
    }
    if !tag.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return Some("only a-z, 0-9 and _");
    }
    if RESERVED.contains(&tag) {
        return Some("reserved");
    }
    None
}

/// Parse an arcade payload as a tag op. None = not a tag message;
/// Some(Err) = a tag message that is invalid (recorded as invalid).
pub fn parse(payload: &[u8]) -> Option<Result<TagOp, &'static str>> {
    if payload.len() < 4 || u16::from_be_bytes([payload[2], payload[3]]) != TYPE_ANY_DATA {
        return None;
    }
    let d = &payload[4..];
    if d.len() < 6 || &d[..4] != b"INSC" || d[4] != 1 {
        return None;
    }
    let kind = d[5];
    if kind != KIND_CLAIM && kind != KIND_TRANSFER {
        return None;
    }
    Some((|| {
        let len = *d.get(6).ok_or("truncated")? as usize;
        let raw = d.get(7..7 + len).ok_or("truncated")?;
        let tag = std::str::from_utf8(raw).map_err(|_| "not utf-8")?.to_string();
        if let Some(p) = tag_problem(&tag) {
            return Err(p);
        }
        Ok(if kind == KIND_CLAIM { TagOp::Claim(tag) } else { TagOp::Transfer(tag) })
    })())
}

/// Build the payload for a claim (what the wallet puts after the "arcd" marker).
pub fn claim_payload(tag: &str) -> Vec<u8> {
    let mut p = vec![0, 0];
    p.extend_from_slice(&TYPE_ANY_DATA.to_be_bytes());
    p.extend_from_slice(b"INSC");
    p.extend_from_slice(&[1, KIND_CLAIM, tag.len() as u8]);
    p.extend_from_slice(tag.as_bytes());
    p
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRow {
    pub address: String,
    pub claimed_txid: String,
    pub block_height: u32,
    pub position: u32,
}

/// Storage for the tag table: tag PRIMARY KEY, address UNIQUE.
pub trait TagStore {
    type E;
    fn tag(&self, tag: &str) -> Result<Option<TagRow>, Self::E>;
    fn tag_of(&self, address: &str) -> Result<Option<String>, Self::E>;
    fn set(&mut self, tag: &str, row: TagRow) -> Result<(), Self::E>;
    fn remove(&mut self, tag: &str, address: &str) -> Result<(), Self::E>;
}

/// Apply one op from `sender` (with `reference`). Ok(Err) = invalid, nothing changed.
pub fn apply<S: TagStore>(
    s: &mut S,
    op: &TagOp,
    sender: &str,
    reference: Option<&str>,
    txid: &str,
    height: u32,
    position: u32,
) -> Result<Result<(), &'static str>, S::E> {
    let row = |address: &str| TagRow { address: address.into(), claimed_txid: txid.into(), block_height: height, position };
    match op {
        TagOp::Claim(t) => {
            if let Some(r) = s.tag(t)? {
                return Ok(Err(if r.address == sender { "already yours" } else { "taken" }));
            }
            if let Some(old) = s.tag_of(sender)? {
                s.remove(&old, sender)?;
            }
            s.set(t, row(sender))?;
            Ok(Ok(()))
        }
        TagOp::Transfer(t) => {
            match s.tag(t)? {
                Some(r) if r.address == sender => {}
                _ => return Ok(Err("not yours")),
            }
            let Some(to) = reference else { return Ok(Err("no recipient")) };
            if to == sender {
                return Ok(Err("to yourself"));
            }
            if s.tag_of(to)?.is_some() {
                return Ok(Err("recipient already has a tag"));
            }
            s.remove(t, sender)?;
            s.set(t, row(to))?;
            Ok(Ok(()))
        }
    }
}

// ---------------- expiry (docs/ARCADECHAIN.md §15 step 8; the ARCADE chain only) ----------------
//
// On a chain that turns it on, a tag is kept by activity: any arcade record from its owner's address (a check-in, kind
// 20, is the record for an owner with nothing else to send, and pays at least `checkin_fee` into the pot). `idle` after
// the last activity, then `grace`, it is released. A released tag is claimed again only with a commitment made ahead:
// during `reveal_blocks` after the release a claim must carry the salt of a commit record (kind 21,
// `commitment(tag, address, salt)`) from an earlier block, and when the window ends the earliest commitment wins; after
// the window the first claim wins again. The home chain's tags never expire (nothing here runs there).

use std::sync::RwLock;

/// (INSC kinds 5-8 are the inscription market's; the tag kinds go from 20.)
pub const KIND_CHECKIN: u8 = 20;
pub const KIND_COMMIT: u8 = 21;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expiry {
    /// Seconds without activity before the grace begins.
    pub idle: u64,
    /// Seconds of grace: still the owner's, flagged as expiring.
    pub grace: u64,
    /// Blocks after a release in which claims need a commitment (the earliest wins).
    pub reveal_blocks: u32,
    /// Least fee of a check-in's transaction.
    pub checkin_fee: u64,
    /// Seconds after a released tag changes owner that a send to it warns.
    pub warn: u64,
}

impl Expiry {
    /// The released time of a tag last active at `t`.
    pub fn due(&self, t: u64) -> u64 {
        t.saturating_add(self.idle).saturating_add(self.grace)
    }
}

static EXPIRY: RwLock<BTreeMap<u16, Expiry>> = RwLock::new(BTreeMap::new());

/// Turn expiry on (or off) for chain `c`: set by the ARCADE chain's service when it starts, before any block is indexed.
pub fn set_expiry(c: u16, e: Option<Expiry>) {
    let mut m = EXPIRY.write().unwrap();
    match e {
        Some(e) => m.insert(c, e),
        None => m.remove(&c),
    };
}

pub fn expiry(c: u16) -> Option<Expiry> {
    EXPIRY.read().unwrap().get(&c).copied()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpiryOp {
    CheckIn(String),
    Commit([u8; 32]),
}

/// A check-in or commit record (kinds 5 and 6), on a chain with expiry. None = neither.
pub fn parse_expiry_op(payload: &[u8]) -> Option<Result<ExpiryOp, &'static str>> {
    if payload.len() < 4 || u16::from_be_bytes([payload[2], payload[3]]) != TYPE_ANY_DATA {
        return None;
    }
    let d = &payload[4..];
    if d.len() < 6 || &d[..4] != b"INSC" || d[4] != 1 || !(d[5] == KIND_CHECKIN || d[5] == KIND_COMMIT) {
        return None;
    }
    Some((|| {
        let len = *d.get(6).ok_or("truncated")? as usize;
        let raw = d.get(7..7 + len).ok_or("truncated")?;
        if d[5] == KIND_COMMIT {
            return Ok(ExpiryOp::Commit(raw.try_into().map_err(|_| "a commitment is 32 bytes")?));
        }
        let tag = std::str::from_utf8(raw).map_err(|_| "not utf-8")?.to_string();
        if let Some(p) = tag_problem(&tag) {
            return Err(p);
        }
        Ok(ExpiryOp::CheckIn(tag))
    })())
}

fn kind_payload(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut p = vec![0, 0];
    p.extend_from_slice(&TYPE_ANY_DATA.to_be_bytes());
    p.extend_from_slice(b"INSC");
    p.extend_from_slice(&[1, kind, body.len() as u8]);
    p.extend_from_slice(body);
    p
}

pub fn checkin_payload(tag: &str) -> Vec<u8> {
    kind_payload(KIND_CHECKIN, tag.as_bytes())
}

pub fn commit_payload(commitment: &[u8; 32]) -> Vec<u8> {
    kind_payload(KIND_COMMIT, commitment)
}

/// A claim carrying the salt of its commitment (after the tag; readers that don't know it ignore it).
pub fn claim_payload_salted(tag: &str, salt: &[u8]) -> Vec<u8> {
    let mut p = claim_payload(tag);
    p.push(salt.len() as u8);
    p.extend_from_slice(salt);
    p
}

/// The salt a claim carries, if any.
pub fn claim_salt(payload: &[u8]) -> Option<Vec<u8>> {
    let d = payload.get(4..)?;
    let len = *d.get(6)? as usize;
    let at = 7 + len;
    let n = *d.get(at)? as usize;
    d.get(at + 1..at + 1 + n).map(|s| s.to_vec())
}

/// What a commit record names: `sha256("arcade tag commit" 0 tag 0 address 0 salt)`.
pub fn commitment(tag: &str, address: &str, salt: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    for part in [b"arcade tag commit".as_slice(), tag.as_bytes(), address.as_bytes(), salt] {
        h.update(part);
        h.update([0]);
    }
    h.finalize().into()
}

/// names_digest over rows in tag order: sha256 of "{tag}|{address}|{block_height}\n".
pub fn digest<'a>(rows: impl Iterator<Item = (&'a str, &'a TagRow)>) -> String {
    let mut h = Sha256::new();
    for (t, r) in rows {
        h.update(format!("{t}|{}|{}\n", r.address, r.block_height).as_bytes());
    }
    hex::encode(h.finalize())
}

/// In-memory tag table (replays, tests).
#[derive(Debug, Default, Clone)]
pub struct Tags {
    pub by_tag: BTreeMap<String, TagRow>,
    pub by_addr: BTreeMap<String, String>,
}

impl TagStore for Tags {
    type E = std::convert::Infallible;
    fn tag(&self, t: &str) -> Result<Option<TagRow>, Self::E> {
        Ok(self.by_tag.get(t).cloned())
    }
    fn tag_of(&self, a: &str) -> Result<Option<String>, Self::E> {
        Ok(self.by_addr.get(a).cloned())
    }
    fn set(&mut self, t: &str, row: TagRow) -> Result<(), Self::E> {
        self.by_addr.insert(row.address.clone(), t.into());
        self.by_tag.insert(t.into(), row);
        Ok(())
    }
    fn remove(&mut self, t: &str, a: &str) -> Result<(), Self::E> {
        self.by_tag.remove(t);
        self.by_addr.remove(a);
        Ok(())
    }
}

impl Tags {
    pub fn apply(&mut self, op: &TagOp, sender: &str, reference: Option<&str>, txid: &str, height: u32, position: u32) -> Result<(), &'static str> {
        match apply(self, op, sender, reference, txid, height, position) {
            Ok(r) => r,
            Err(never) => match never {},
        }
    }

    pub fn digest(&self) -> String {
        digest(self.by_tag.iter().map(|(t, r)| (t.as_str(), r)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claim_roundtrip_and_rules() {
        assert_eq!(parse(&claim_payload("martin")), Some(Ok(TagOp::Claim("martin".into()))));
        assert_eq!(parse(&claim_payload("Martin")), Some(Err("only a-z, 0-9 and _")));
        assert_eq!(parse(&claim_payload("a")), Some(Err("too short")));
        assert_eq!(parse(&claim_payload("admin")), Some(Err("reserved")));
        let mut t = claim_payload("abc");
        t.truncate(t.len() - 1);
        assert_eq!(parse(&t), Some(Err("truncated")));
        assert_eq!(parse(&[0, 0, 0, 0]), None);
    }

    #[test]
    fn expiry_records_read_back_and_stay_unknown_to_plain_readers() {
        let c = commitment("bob", "aXyz", b"salt");
        assert_ne!(c, commitment("bob", "aXyz", b"salu"));
        assert_ne!(c, commitment("bob", "aXy", b"zsalt"), "parts are separated");
        assert_eq!(parse_expiry_op(&commit_payload(&c)), Some(Ok(ExpiryOp::Commit(c))));
        assert_eq!(parse_expiry_op(&checkin_payload("bob")), Some(Ok(ExpiryOp::CheckIn("bob".into()))));
        assert_eq!(parse_expiry_op(&checkin_payload("Bob")), Some(Err("only a-z, 0-9 and _")));
        // The home chain's reader doesn't know kinds 20 and 21 (nothing changes there).
        assert_eq!(parse(&commit_payload(&c)), None);
        assert_eq!(parse(&checkin_payload("bob")), None);
        // A salted claim is still a claim to every reader, and its salt reads back.
        let salted = claim_payload_salted("bob", b"0123456789abcdef");
        assert_eq!(parse(&salted), Some(Ok(TagOp::Claim("bob".into()))));
        assert_eq!(claim_salt(&salted).as_deref(), Some(&b"0123456789abcdef"[..]));
        assert_eq!(claim_salt(&claim_payload("bob")), None);
        let x = Expiry { idle: 10, grace: 5, reveal_blocks: 3, checkin_fee: 1, warn: 1 };
        assert_eq!(x.due(100), 115);
        set_expiry(9_999, Some(x));
        assert_eq!(expiry(9_999), Some(x));
        set_expiry(9_999, None);
        assert_eq!(expiry(9_999), None);
    }

    #[test]
    fn claim_and_transfer_rules() {
        let mut t = Tags::default();
        let c = |s: &str| TagOp::Claim(s.into());
        t.apply(&c("alice"), "A", None, "t1", 10, 1).unwrap();
        assert_eq!(t.apply(&c("alice"), "A", None, "t2", 11, 1), Err("already yours"));
        assert_eq!(t.apply(&c("alice"), "B", None, "t3", 11, 2), Err("taken"));
        // A claims a new tag: the old one is released.
        t.apply(&c("alice2"), "A", None, "t4", 12, 1).unwrap();
        assert!(!t.by_tag.contains_key("alice"));
        t.apply(&c("bob"), "B", None, "t5", 13, 1).unwrap();
        let tr = TagOp::Transfer("alice2".into());
        assert_eq!(t.apply(&tr, "A", Some("B"), "t6", 14, 1), Err("recipient already has a tag"));
        t.apply(&tr, "A", Some("C"), "t7", 14, 2).unwrap();
        assert_eq!(t.by_addr.get("C").map(String::as_str), Some("alice2"));
        assert!(!t.by_addr.contains_key("A"));
        // Empty table digest is sha256("").
        assert_eq!(
            Tags::default().digest(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
