//! The ARCADE faucet (Martin, 2026-11-15): a user who signs in gets their ARCADE address
//! topped up to 0.05 ARCADE once a day (0.01 until 2026-11-16: too little for a comment, whose text blob costs about
//! 0.02), so they can publish an a:// profile, like, comment and tip without
//! buying coins first. Any arcade on the mesh with spare test ARCADE pays.
//!
//! Mesh frame `t:"af"`, a kind of its own (not the welcome protocol's `wl`, which old arcades
//! answer with test PEP). Records are Ed25519-signed by the arcade's node key `a`, as `wl`'s:
//!   ask  {a, addr, at, k:"ask"}         top `addr` up to TARGET, please (one per address a day)
//!   sent {a, addr, at, k:"sent", txid}  paid it in `txid`
//!   welcome {a, addr, at, k:"welcome"}  a new wallet: give `addr` WELCOME (1 ARCADE), once, if it never held ARCADE
//! A new arcade so gets two transactions off the rip (Martin 2026-11-17): its @handle claim's fee (test PEP from the
//! welcome faucet, or ARCADE from a top-up when ARCADE is its claim home), and the 1 ARCADE welcome to like, comment
//! and tip with.
//! Asks are kept per address, so one arcade (a website with many users) may ask for many.
//!
//! The payer looks at the address's balance on its own copy of the chain (blocks and pool) and
//! sends only what is missing to TARGET, so a second payer that comes later sends nothing.
//! Limits per payer and UTC day: DAILY_CAP top-ups, PER_ASKER of them for one asking arcade, and
//! it keeps KEEP for itself.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

/// What a signed-in user's ARCADE address is topped up to: 0.05 ARCADE (about two comments, or eighty likes).
pub const TARGET: u64 = 5_000_000;
/// Top-ups one arcade pays per UTC day (at most 25 ARCADE a day).
pub const DAILY_CAP: usize = 500;
/// Of those, for addresses one asking arcade named.
pub const PER_ASKER: usize = 200;
/// What a new wallet's ARCADE address gets once (k:"welcome"): 1 ARCADE.
pub const WELCOME: u64 = 100_000_000;
/// Welcomes one arcade pays per UTC day (at most 100 ARCADE a day).
pub const WELCOME_DAILY_CAP: usize = 100;
/// Of those, for addresses one asking arcade named (an arcade has one wallet; a few restarts or test wallets).
pub const WELCOME_PER_ASKER: usize = 3;
/// Never given away: kept for the payer's own a:// records.
pub const KEEP: u64 = 100_000_000;
/// Kept back for the payment's own fee.
const FEE_MARGIN: u64 = 1_000_000;
/// An ask counts this long.
const ASK_SECS: u64 = 86_400;
/// Clock difference allowed for a record dated ahead.
const SKEW_SECS: u64 = 600;
/// A payer waits a share of this after an ask, different per payer, so payers don't all pay
/// the same ask at once (the one that comes later sees the coins and sends nothing).
const SPREAD_SECS: u64 = 60;
/// A payment notice keeps others from paying the address for this long; after it the balance
/// alone decides (a notice is only its sender's word).
const SENT_HOLD: u64 = 600;
/// Most addresses kept with an open ask, and with payment notices: past it a new one is
/// neither kept nor passed on (asks are signed, but node keys cost nothing to make).
const MAX_ADDRS: usize = 20_000;

#[derive(Default)]
struct State {
    /// The newest ask per address.
    asks: HashMap<String, Value>,
    /// Payment notices seen per address: (txid, at).
    sent: HashMap<String, Vec<(String, u64)>>,
    /// Addresses found at TARGET or more (or paid) by this arcade, with when.
    done: HashMap<String, u64>,
    /// The newest welcome ask per address.
    welcomes: HashMap<String, Value>,
    /// Addresses this arcade found had ARCADE before, or welcomed: never welcomed (again).
    welcomed: HashSet<String>,
    /// When this arcade last asked a welcome for its own wallet (seconds).
    welcome_asked: u64,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// The ARCADE chain running here, or why there is none.
fn chain() -> Result<&'static ChainParams, String> {
    crate::arcledger::chain()
}

fn script_of(p: &ChainParams, addr: &str) -> Result<Vec<u8>, String> {
    arcade_ledger::address::p2pkh_script_for_address(p, addr).ok_or_else(|| format!("{addr} is not an {} address", p.name))
}

/// The address's ARCADE here: confirmed and waiting, not spent by a waiting transaction.
fn balance(node: &Node, p: &ChainParams, addr: &str) -> Option<u64> {
    let v = crate::sitesvc::coins(node, &json!({"chain": p.key, "address": addr})).ok()?;
    v["addresses"][addr]["total"].as_u64()
}

fn day_key(prefix: &str, now: u64) -> Vec<u8> {
    format!("{prefix}:{}", now / 86_400).into_bytes()
}

fn read_list(node: &Node, key: &[u8]) -> Vec<Value> {
    node.app.store.read(|r| r.get(arcade_app::META, key)).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or_default()
}

fn write_list(node: &Node, key: &[u8], list: &[Value]) {
    let _ = node.app.store.write(|w| w.put(arcade_app::META, key, &serde_json::to_vec(list).unwrap()));
}

fn frame(rec: Value) -> Value {
    json!({"t": "af", "recs": [rec]})
}

/// Ask the mesh to top `addr` up to 0.05 ARCADE: once per address and UTC day, and only when it
/// has less than that (as far as this arcade can see). This arcade may pay its own ask too.
pub fn ask(node: &Node, addr: &str) -> Result<Value, String> {
    let addr = addr.trim();
    let now = arcade_app::now();
    // Without the chain here the address is checked by its form only (the payer checks again).
    let p = crate::arcsvc::defined().ok_or("this arcade doesn't know the ARCADE chain")?;
    script_of(p, addr)?;
    if let Ok(p) = chain() {
        if let Some(have) = balance(node, p, addr).filter(|h| *h + arcade_arc::params::DUST > TARGET) {
            return Ok(json!({"asked": false, "address": addr, "balance": have, "why": "it has 0.05 ARCADE already"}));
        }
    }
    let key = day_key("arc_faucet_asked", now);
    let mut asked = read_list(node, &key);
    if asked.iter().any(|a| a == addr) {
        return Ok(json!({"asked": false, "address": addr, "why": "asked for it today already"}));
    }
    let rec = arcade_mesh::welcome::sign(json!({"a": node.id_hex(), "addr": addr, "at": now, "k": "ask"}), &node.identity);
    state().lock().unwrap().asks.insert(addr.to_string(), rec.clone());
    node.mesh_outbox.lock().unwrap().push(frame(rec));
    asked.push(json!(addr));
    write_list(node, &key, &asked);
    let _ = node.app.event("faucet", &format!("asked the mesh to top {addr} up to 0.05 ARCADE"));
    Ok(json!({"asked": true, "address": addr, "target": TARGET}))
}

/// Ask the mesh to welcome `addr` with 1 ARCADE: once per address and UTC day until it has ARCADE (as far as this
/// arcade can see; a payer checks again on its own copy, and pays only an address that never held ARCADE).
pub fn ask_welcome(node: &Node, addr: &str) -> Result<Value, String> {
    let addr = addr.trim();
    let now = arcade_app::now();
    let p = crate::arcsvc::defined().ok_or("this arcade doesn't know the ARCADE chain")?;
    script_of(p, addr)?;
    if let Ok(p) = chain() {
        if held_before(node, p, addr) {
            return Ok(json!({"asked": false, "address": addr, "why": "it has had its welcome ARCADE already"}));
        }
    }
    let key = day_key("arc_welcome_asked", now);
    let mut asked = read_list(node, &key);
    if asked.iter().any(|a| a == addr) {
        return Ok(json!({"asked": false, "address": addr, "why": "asked for it today already"}));
    }
    let rec = arcade_mesh::welcome::sign(json!({"a": node.id_hex(), "addr": addr, "at": now, "k": "welcome"}), &node.identity);
    state().lock().unwrap().welcomes.insert(addr.to_string(), rec.clone());
    node.mesh_outbox.lock().unwrap().push(frame(rec));
    asked.push(json!(addr));
    write_list(node, &key, &asked);
    let _ = node.app.event("faucet", &format!("asked the mesh to welcome {addr} with 1 ARCADE"));
    Ok(json!({"asked": true, "address": addr, "welcome": WELCOME}))
}

/// This wallet's ARCADE address (its first key's): asked a welcome for, when the wallet is new or still has none.
/// Called on wallet creation and every few minutes after (an ask made before the mesh linked is made again).
pub fn welcome_wallet(node: &Node) {
    let now = arcade_app::now();
    {
        let mut st = state().lock().unwrap();
        if now.saturating_sub(st.welcome_asked) < 300 {
            return;
        }
        st.welcome_asked = now;
    }
    if !node.app.has_wallet().unwrap_or(false) || node.links.lock().unwrap().is_empty() {
        state().lock().unwrap().welcome_asked = 0;
        return;
    }
    let Some(p) = crate::arcsvc::defined() else { return };
    if let Ok((k, _)) = node.app.tag_key(p, 0) {
        if state().lock().unwrap().welcomed.contains(&k.address) {
            return;
        }
        // Found past it (holding a welcome's worth, here or on a lender's copy): done for good.
        let has = chain().ok().is_some_and(|c| held_before(node, c, &k.address))
            || crate::sitesvc::coins(node, &json!({"chain": p.key, "address": k.address})).ok().and_then(|v| v["addresses"][&k.address]["total"].as_u64()).is_some_and(|t| t >= WELCOME / 2);
        if has {
            state().lock().unwrap().welcomed.insert(k.address.clone());
            return;
        }
        let _ = ask_welcome(node, &k.address);
    }
}

/// Whether `addr` is past its welcome on this arcade's copy: it received more than top-ups would bring (TARGET in
/// all), or holds a welcome's worth now (the pool counted). A claim's top-up may come first: the welcome still comes.
fn held_before(node: &Node, p: &ChainParams, addr: &str) -> bool {
    let Ok(script) = script_of(p, addr) else { return true };
    let Ok(hist) = node.chain.history_for_script(p.chain_no, &script) else { return true };
    let Some(have) = balance(node, p, addr) else { return true };
    past_welcome(hist.iter().map(|(_, _, recv, _)| *recv).sum(), have)
}

/// [`held_before`] from what the address received in all and holds now.
fn past_welcome(received: u64, have: u64) -> bool {
    received > TARGET || have >= WELCOME / 2
}

/// After a sign-in: ask for the account's ARCADE address (its first key's), when it is low.
pub fn ask_for_wallet(node: &Node) {
    let Some(p) = crate::arcsvc::defined() else { return };
    if let Ok((k, _)) = node.app.tag_key(p, 0) {
        let _ = ask(node, &k.address);
    }
}

fn fresh(at: u64, now: u64) -> bool {
    at <= now + SKEW_SECS && now.saturating_sub(at) < ASK_SECS
}

/// An `af` frame arrived: keep asks and payment notices; pass each new one on once.
pub fn on_frame(node: &Node, head: &Value) {
    let now = arcade_app::now();
    let Some(recs) = head["recs"].as_array() else { return };
    for r in recs {
        if !arcade_mesh::welcome::verify(r) {
            continue;
        }
        let (Some(addr), Some(at)) = (r["addr"].as_str(), r["at"].as_u64()) else { continue };
        if !fresh(at, now) || addr.len() > 64 {
            continue;
        }
        // Only an ARCADE address is kept and passed on.
        if crate::arcsvc::defined().is_some_and(|p| script_of(p, addr).is_err()) {
            continue;
        }
        let new = {
            let mut st = state().lock().unwrap();
            match r["k"].as_str() {
                Some("ask") => {
                    if !st.asks.contains_key(addr) && st.asks.len() >= MAX_ADDRS {
                        continue;
                    }
                    let newer = st.asks.get(addr).map(|o| o["at"].as_u64() < Some(at)).unwrap_or(true);
                    if newer {
                        st.asks.insert(addr.to_string(), r.clone());
                    }
                    newer
                }
                Some("welcome") => {
                    if !st.welcomes.contains_key(addr) && st.welcomes.len() >= MAX_ADDRS {
                        continue;
                    }
                    let newer = st.welcomes.get(addr).map(|o| o["at"].as_u64() < Some(at)).unwrap_or(true);
                    if newer {
                        st.welcomes.insert(addr.to_string(), r.clone());
                    }
                    newer
                }
                Some("sent") => {
                    let Some(txid) = r["txid"].as_str() else { continue };
                    if !st.sent.contains_key(addr) && st.sent.len() >= MAX_ADDRS {
                        continue;
                    }
                    let list = st.sent.entry(addr.to_string()).or_default();
                    let new = !list.iter().any(|(t, _)| t == txid);
                    if new {
                        list.push((txid.to_string(), at));
                        if list.len() > 8 {
                            list.remove(0);
                        }
                    }
                    new
                }
                _ => false,
            }
        };
        if new {
            node.mesh_outbox.lock().unwrap().push(frame(r.clone()));
        }
    }
}

/// How long after an ask this arcade (`me`) waits before paying it: fixed per pair.
fn delay(me: &str, addr: &str) -> u64 {
    let h = Sha256::digest(format!("{me}:{addr}").as_bytes());
    u64::from_le_bytes(h[..8].try_into().unwrap()) % SPREAD_SECS
}

/// What to send an address that has `have`: what is missing to TARGET, or nothing when that is
/// less than the chain's smallest output.
fn top_up(have: u64) -> Option<u64> {
    let need = TARGET.saturating_sub(have);
    (need >= arcade_arc::params::DUST).then_some(need)
}

/// The most this wallet can spend from one key on ARCADE (a send pays from one key).
fn spendable(node: &Node, p: &ChainParams) -> u64 {
    let mut keys = node.app.tag_keys(p.chain_no).unwrap_or_default();
    if keys.is_empty() {
        if let Ok((k, _)) = node.app.tag_key(p, 0) {
            keys.push(k);
        }
    }
    keys.iter()
        .filter_map(|k| arcade_ledger::address::p2pkh_script_for_address(p, &k.address))
        .map(|s| crate::claims::arc_coins(node, p, &s).iter().map(|u| u.value).sum::<u64>())
        .max()
        .unwrap_or(0)
}

/// Pay one open ask from this arcade's wallet, if it can (called every 15 seconds).
pub fn pay(node: &Node) {
    let Ok(p) = chain() else { return };
    // Only from its own copy, caught up: the balance it reads must be the chain's.
    if crate::arcsvc::lends().is_none() || !node.app.has_wallet().unwrap_or(false) {
        return;
    }
    if pay_welcome(node, p) {
        return; // one payment per pass
    }
    let now = arcade_app::now();
    let me = node.id_hex();
    let key = day_key("arc_faucet_paid", now);
    let paid = read_list(node, &key); // [[addr, asker], ...]
    if paid.len() >= DAILY_CAP {
        return;
    }
    let candidates: Vec<(String, String, u64)> = {
        let mut st = state().lock().unwrap();
        st.asks.retain(|_, r| r["at"].as_u64().is_some_and(|t| fresh(t, now)));
        st.done.retain(|_, t| now.saturating_sub(*t) < ASK_SECS);
        st.sent.retain(|_, l| {
            l.retain(|(_, at)| now.saturating_sub(*at) < ASK_SECS);
            !l.is_empty()
        });
        st.asks
            .iter()
            .filter_map(|(addr, r)| {
                let at = r["at"].as_u64()?;
                let asker = r["a"].as_str()?.to_string();
                // Done since this ask, or a payment announced lately.
                if st.done.get(addr).is_some_and(|t| *t >= at) {
                    return None;
                }
                if st.sent.get(addr).is_some_and(|l| l.iter().any(|(_, t)| *t + SKEW_SECS >= at && now.saturating_sub(*t) < SENT_HOLD)) {
                    return None;
                }
                (now >= at + delay(&me, addr)).then_some((addr.clone(), asker, at))
            })
            .collect()
    };
    let have_spare = spendable(node, p).saturating_sub(KEEP + FEE_MARGIN);
    for (addr, asker, _) in candidates {
        let by_asker = paid.iter().filter(|e| e[1] == asker.as_str()).count();
        if by_asker >= PER_ASKER || paid.iter().any(|e| e[0] == addr.as_str()) {
            continue;
        }
        let Some(have) = balance(node, p, &addr) else { continue };
        let Some(amount) = top_up(have) else {
            state().lock().unwrap().done.insert(addr, now);
            continue;
        };
        if amount > have_spare {
            return; // nothing to spare: someone else pays
        }
        let r = match crate::coinsend::send(node, Some(p.key), &addr, amount, None, None, false, true) {
            Ok(r) => r,
            Err(e) => {
                let _ = node.app.event("faucet", &format!("could not top {addr} up with ARCADE: {e}"));
                return;
            }
        };
        let txid = r["broadcast"]["txid"].as_str().or(r["txid"].as_str()).unwrap_or("").to_string();
        let sent = arcade_mesh::welcome::sign(json!({"a": me, "addr": addr, "at": now, "k": "sent", "txid": txid}), &node.identity);
        node.mesh_outbox.lock().unwrap().push(frame(sent));
        {
            let mut st = state().lock().unwrap();
            st.done.insert(addr.clone(), now);
            st.sent.entry(addr.clone()).or_default().push((txid.clone(), now));
        }
        let mut list = paid;
        list.push(json!([addr, asker]));
        write_list(node, &key, &list);
        let _ = node.app.event("faucet", &format!("topped {addr} up with {} ARCADE in {txid}", amount as f64 / 1e8));
        return; // one per pass: the next pass sees the updated coins
    }
}

/// Pay one open welcome ask from this arcade's wallet, if it can: 1 ARCADE to an address that never held ARCADE.
/// Whether it paid.
fn pay_welcome(node: &Node, p: &ChainParams) -> bool {
    let now = arcade_app::now();
    let me = node.id_hex();
    let key = day_key("arc_welcome_paid", now);
    let paid = read_list(node, &key); // [[addr, asker], ...]
    if paid.len() >= WELCOME_DAILY_CAP {
        return false;
    }
    let candidates: Vec<(String, String)> = {
        let mut st = state().lock().unwrap();
        st.welcomes.retain(|_, r| r["at"].as_u64().is_some_and(|t| fresh(t, now)));
        let welcomed = st.welcomed.clone();
        let sent = st.sent.clone();
        st.welcomes
            .iter()
            .filter(|(addr, _)| !welcomed.contains(*addr))
            .filter_map(|(addr, r)| {
                let at = r["at"].as_u64()?;
                // A payment announced lately (another payer's welcome or top-up): its coins decide once they arrive.
                if sent.get(addr).is_some_and(|l| l.iter().any(|(_, t)| now.saturating_sub(*t) < SENT_HOLD)) {
                    return None;
                }
                (now >= at + delay(&me, addr)).then(|| (addr.clone(), r["a"].as_str().unwrap_or("").to_string()))
            })
            .collect()
    };
    let have_spare = spendable(node, p).saturating_sub(KEEP + FEE_MARGIN);
    for (addr, asker) in candidates {
        if paid.iter().filter(|e| e[1] == asker.as_str()).count() >= WELCOME_PER_ASKER || paid.iter().any(|e| e[0] == addr.as_str()) {
            continue;
        }
        if held_before(node, p, &addr) {
            state().lock().unwrap().welcomed.insert(addr);
            continue;
        }
        if WELCOME > have_spare {
            return false; // nothing to spare: someone else pays
        }
        let r = match crate::coinsend::send(node, Some(p.key), &addr, WELCOME, None, None, false, true) {
            Ok(r) => r,
            Err(e) => {
                let _ = node.app.event("faucet", &format!("could not welcome {addr} with ARCADE: {e}"));
                return false;
            }
        };
        let txid = r["broadcast"]["txid"].as_str().or(r["txid"].as_str()).unwrap_or("").to_string();
        let sent = arcade_mesh::welcome::sign(json!({"a": me, "addr": addr, "at": now, "k": "sent", "txid": txid}), &node.identity);
        node.mesh_outbox.lock().unwrap().push(frame(sent));
        {
            let mut st = state().lock().unwrap();
            st.welcomed.insert(addr.clone());
            st.sent.entry(addr.clone()).or_default().push((txid.clone(), now));
        }
        let mut list = paid;
        list.push(json!([addr, asker]));
        write_list(node, &key, &list);
        let _ = node.app.event("faucet", &format!("welcomed {addr} with 1 ARCADE in {txid}"));
        return true;
    }
    false
}

/// For the Home tab: `addr`'s top-up as the welcome faucet's is shown ({status: "paid", txid, by} once a payment notice
/// was seen, {status: "asked", at} while asked), or None.
pub fn of_address(addr: &str) -> Option<Value> {
    let st = state().lock().unwrap();
    if let Some((txid, at)) = st.sent.get(addr).and_then(|l| l.last()) {
        return Some(json!({"status": "paid", "at": at, "txid": txid, "by": Value::Null}));
    }
    st.asks.get(addr).map(|r| json!({"status": "asked", "at": r["at"], "txid": Value::Null, "by": r["a"]}))
}

/// `faucet.arc_status`: the asks this arcade knows and what it paid today.
pub fn status(node: &Node) -> Value {
    let now = arcade_app::now();
    let st = state().lock().unwrap();
    let asks: Vec<Value> = st.asks.iter().map(|(a, r)| json!({"address": a, "asker": r["a"], "at": r["at"], "done": st.done.contains_key(a), "notices": st.sent.get(a).map(|l| l.len()).unwrap_or(0)})).collect();
    let paid = read_list(node, &day_key("arc_faucet_paid", now));
    let askers: HashSet<&str> = paid.iter().filter_map(|e| e[1].as_str()).collect();
    let welcomes: Vec<Value> = st.welcomes.iter().map(|(a, r)| json!({"address": a, "asker": r["a"], "at": r["at"], "done": st.welcomed.contains(a)})).collect();
    let welcomed_today = read_list(node, &day_key("arc_welcome_paid", now)).len();
    json!({"welcome": WELCOME, "welcome_daily_cap": WELCOME_DAILY_CAP, "welcomes": welcomes, "welcomed_today": welcomed_today,
           "target": TARGET, "daily_cap": DAILY_CAP, "per_asker": PER_ASKER, "keep": KEEP,
           "running": chain().is_ok(), "caught_up": crate::arcsvc::lends().is_some(),
           "asks": asks, "paid_today": paid.len(), "askers_paid_today": askers.len()})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tops_up_to_the_target() {
        assert_eq!(top_up(0), Some(TARGET));
        assert_eq!(top_up(400_000), Some(TARGET - 400_000));
        assert_eq!(top_up(TARGET), None);
        assert_eq!(top_up(TARGET * 3), None);
        // Short by less than the smallest output: nothing to send.
        assert_eq!(top_up(TARGET - arcade_arc::params::DUST + 1), None);
        assert_eq!(top_up(TARGET - arcade_arc::params::DUST), Some(arcade_arc::params::DUST));
    }

    #[test]
    fn a_claim_top_up_doesnt_use_up_the_welcome() {
        assert!(!past_welcome(0, 0), "new");
        assert!(!past_welcome(TARGET, TARGET), "a claim's top-up came first");
        assert!(!past_welcome(TARGET, 1_000_000), "and the claim spent most of it");
        assert!(past_welcome(TARGET + WELCOME, 3_000_000), "welcomed, then spent");
        assert!(past_welcome(0, WELCOME), "the welcome waits in the pool");
    }

    #[test]
    fn asks_age_out() {
        assert!(fresh(1000, 1000));
        assert!(fresh(1000 + SKEW_SECS, 1000), "a little ahead of our clock");
        assert!(!fresh(1000 + SKEW_SECS + 1, 1000));
        assert!(!fresh(1000, 1000 + ASK_SECS));
    }

    #[test]
    fn payers_wait_differently() {
        let d: HashSet<u64> = (0..20).map(|i| delay(&format!("payer{i}"), "aXeV2c4YTSXLHVHRP5EdfYqeRX9oU4et2e")).collect();
        assert!(d.iter().all(|x| *x < SPREAD_SECS));
        assert!(d.len() > 5, "spread over the window");
    }

    #[test]
    fn records_verify() {
        let k = ed25519_dalek::SigningKey::from_bytes(&[5; 32]);
        let rec = arcade_mesh::welcome::sign(json!({"a": hex::encode(k.verifying_key().to_bytes()), "addr": "aX", "at": 1, "k": "ask"}), &k);
        assert!(arcade_mesh::welcome::verify(&rec));
        let mut bad = rec.clone();
        bad["addr"] = json!("aY");
        assert!(!arcade_mesh::welcome::verify(&bad));
    }
}
