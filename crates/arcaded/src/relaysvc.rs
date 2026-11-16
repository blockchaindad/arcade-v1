//! **Relayed links**: two arcades that can't dial each other (one only reaches names, the other is behind a router)
//! but both have a link to a relay arcade get a mesh link through it. The relay passes bytes between its two links;
//! the two arcades run the ordinary link handshake over that pipe, so the link is end-to-end encrypted and signed: the
//! relay sees only sealed bytes and can't read, change or pretend to be either side. Martin 2026-11-16 ("build #2"),
//! for the Sesame nodes that reach only Ashburn.
//!
//! On the wire, frames `{"t":"rl", "op": ...}` on a link (arcades without this ignore the kind):
//! - to a relay: `hi` (this arcade takes relayed links), `open {n, to}`, `accept {ch}`, `no {ch, why}`,
//!   `d {ch}` + bytes, `close {ch}`;
//! - from a relay: `offer {on, ids}` (it relays, and these of its links take relayed links), `in {ch, from, ip}`,
//!   `ok {n, ch, ip}`, `fail {n?, ch?, why}`, `rd {ch}` + bytes, `end {ch, why}`.
//!
//! The relay is the operator's choice (off unless turned on, `mesh.relay_set`), with a month's traffic limit and a cap
//! on links at once; it counts what it passes (`mesh.relay`). One hop only: nothing is relayed over a relayed link.
//! Of two arcades the lower node id asks, so a pair opens one link.

use crate::node::Node;
use arcade_mesh::link::{Frame, Link, LinkError};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Bytes in one relayed piece (well inside a link frame's 64 kB).
const PIECE: usize = 16 * 1024;
/// Most relayed links an arcade runs (asked and answered).
const MAX_OWN: usize = 8;
/// Links a relay passes at once by default (the operator can set it).
const DEFAULT_MAX_LINKS: u64 = 32;
/// A month's traffic a relay passes by default, in GB (the operator can set it).
const DEFAULT_LIMIT_GB: f64 = 50.0;
/// Most bytes waiting at the relay for one side of one link: a side that doesn't take them closes the link.
const MAX_WAITING: usize = 1 << 20;
/// Most bytes waiting to go out on this arcade's side of a relayed link before a send waits.
const MAX_OUT: usize = 512 * 1024;
/// An open with no answer from the other side is given up after this.
const OPEN_WAIT: Duration = Duration::from_secs(20);
/// How often an arcade looks for arcades to reach through its relays.
const ROUND_EVERY: Duration = Duration::from_secs(30);
/// An arcade a relay couldn't reach is asked for again after this.
const COOL: u64 = 30 * 60;

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

// ---------------------------------------------------------------- settings and the meter

fn cfg(node: &Node) -> Value {
    node.app.store.read(|r| r.get(arcade_app::META, b"mesh_relay")).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or(json!({}))
}

/// Is this arcade a relay for others? Off unless the operator turns it on.
pub fn relay_on(node: &Node) -> bool {
    cfg(node)["on"] == true
}

fn limit_gb(node: &Node) -> f64 {
    cfg(node)["limit_gb"].as_f64().filter(|g| *g >= 0.0).unwrap_or(DEFAULT_LIMIT_GB)
}

fn max_links(node: &Node) -> u64 {
    cfg(node)["max_links"].as_u64().unwrap_or(DEFAULT_MAX_LINKS)
}

fn this_month() -> String {
    let d = now_s() / 86400;
    let (y, m, _) = civil(d as i64);
    format!("{y:04}-{m:02}")
}

fn today() -> String {
    let (y, m, d) = civil((now_s() / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to (year, month, day) (Howard Hinnant's civil_from_days).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[derive(Default)]
struct Meter {
    month: String,
    month_bytes: u64,
    day: String,
    day_bytes: u64,
    saved: Option<Instant>,
}

impl Meter {
    fn roll(&mut self) {
        let (m, d) = (this_month(), today());
        if self.month != m {
            self.month = m;
            self.month_bytes = 0;
        }
        if self.day != d {
            self.day = d;
            self.day_bytes = 0;
        }
    }
}

// ---------------------------------------------------------------- state

/// A link this arcade passes, as the relay.
struct Chan {
    a: String,
    b: String,
    /// The asker's number for its open, until the other side answers.
    n: Option<String>,
    accepted: bool,
    asked: Instant,
    since: u64,
    bytes: u64,
}

/// This arcade's side of a relayed link: what the relay brought, and what waits to go to it.
#[derive(Default)]
struct EndState {
    inbox: Vec<u8>,
    out: VecDeque<Vec<u8>>,
    out_bytes: usize,
    closed: Option<String>,
}

struct End {
    relay: String,
    ch: u64,
    peer: String,
    p: Mutex<EndState>,
    cv: Condvar,
}

#[derive(Default)]
struct G {
    // As the relay.
    chans: BTreeMap<u64, Chan>,
    next: u64,
    /// Frames for each linked arcade (relay side), with the bytes waiting per (link, side).
    to: HashMap<String, VecDeque<Frame>>,
    waiting: HashMap<(u64, String), usize>,
    /// Linked arcades that take relayed links (they said "hi").
    capable: BTreeSet<String>,
    /// What each link was last offered (on, ids).
    offered: HashMap<String, (bool, Vec<String>)>,
    meter: Meter,
    // As a user of relays.
    /// Relays offering to pass links, and the arcades they reach.
    relays: HashMap<String, Vec<String>>,
    /// Frames for each relay (control: open, accept, no, close).
    ctl: HashMap<String, VecDeque<Frame>>,
    /// Opens asked: n -> (relay, target, when).
    asking: HashMap<String, (String, String, Instant)>,
    ends: HashMap<(String, u64), Arc<End>>,
    /// Arcades not to ask for again before a time.
    cooling: HashMap<String, u64>,
    /// Relayed links up: peer -> relay.
    up: BTreeMap<String, String>,
    /// Links that have said "hi" to (peer ids), so it is said once a link.
    said_hi: BTreeSet<String>,
    last_round: Option<Instant>,
}

fn g() -> &'static Mutex<G> {
    static S: std::sync::OnceLock<Mutex<G>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

fn usage_file(node: &Node) -> std::path::PathBuf {
    node.home.join("relay-usage.json")
}

/// The meter from disk (this month's and today's bytes), once at start.
fn load_meter(node: &Node, m: &mut Meter) {
    if m.saved.is_some() {
        return;
    }
    let v: Value = std::fs::read_to_string(usage_file(node)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}));
    m.month = v["month"].as_str().unwrap_or("").to_string();
    m.month_bytes = v["month_bytes"].as_u64().unwrap_or(0);
    m.day = v["day"].as_str().unwrap_or("").to_string();
    m.day_bytes = v["day_bytes"].as_u64().unwrap_or(0);
    m.roll();
    m.saved = Some(Instant::now());
}

fn save_meter(node: &Node, m: &mut Meter) {
    let v = json!({"month": m.month, "month_bytes": m.month_bytes, "day": m.day, "day_bytes": m.day_bytes});
    let (f, tmp) = (usage_file(node), node.home.join("relay-usage.json.new"));
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &f);
    }
    m.saved = Some(Instant::now());
}

fn over_limit(node: &Node, m: &Meter) -> bool {
    m.month_bytes as f64 >= limit_gb(node) * 1e9
}

fn linked(node: &Node, id: &str) -> bool {
    node.links.lock().unwrap().values().any(|l| l.node_id == id)
}

/// The address a linked arcade is known at here (the IP part: what the other side shows for it).
fn ip_of(node: &Node, id: &str) -> String {
    node.links.lock().unwrap().values().find(|l| l.node_id == id).map(|l| l.addr.rsplit_once(':').map(|x| x.0).unwrap_or(&l.addr).to_string()).unwrap_or_default()
}

fn rl(head: Value) -> Frame {
    let mut h = head;
    h["t"] = json!("rl");
    Frame::new(h)
}

/// A link that is itself relayed: nothing is relayed over it (one hop only).
fn is_relayed(link: &Link) -> bool {
    link.describe().starts_with("relay:")
}

// ---------------------------------------------------------------- per link: frames out

/// Each pass of a link's loop: what waits for this link goes out.
pub fn tick(node: &Node, link: &mut Link) -> Result<(), LinkError> {
    if is_relayed(link) {
        return Ok(());
    }
    round(node);
    let peer = link.peer_id.clone();
    let mut out: Vec<Frame> = Vec::new();
    {
        let mut st = g().lock().unwrap();
        if st.said_hi.insert(peer.clone()) {
            out.push(rl(json!({"op": "hi"})));
        }
        // As the relay: what it offers this link (who among its links takes relayed links), when it changes.
        let on = relay_on(node);
        let ids: Vec<String> = if on { st.capable.iter().filter(|i| **i != peer).cloned().collect() } else { Vec::new() };
        if st.offered.get(&peer).is_none_or(|o| o.0 != on || o.1 != ids) && (on || st.offered.contains_key(&peer)) {
            st.offered.insert(peer.clone(), (on, ids.clone()));
            out.push(rl(json!({"op": "offer", "on": on, "ids": ids})));
        }
        if let Some(q) = st.to.get_mut(&peer) {
            out.extend(q.drain(..));
        }
        let mut freed: Vec<(u64, usize)> = Vec::new();
        for f in &out {
            if f.head["op"] == "rd" {
                freed.push((f.head["ch"].as_u64().unwrap_or(0), f.blob.len()));
            }
        }
        for (ch, n) in freed {
            if let Some(w) = st.waiting.get_mut(&(ch, peer.clone())) {
                *w = w.saturating_sub(n);
            }
        }
        // As a user of this relay: control frames, then each relayed link's bytes.
        if let Some(q) = st.ctl.get_mut(&peer) {
            out.extend(q.drain(..));
        }
        let ends: Vec<Arc<End>> = st.ends.values().filter(|e| e.relay == peer).cloned().collect();
        drop(st);
        for e in ends {
            let mut p = e.p.lock().unwrap();
            while let Some(b) = p.out.pop_front() {
                p.out_bytes -= b.len();
                out.push(Frame { head: json!({"t": "rl", "op": "d", "ch": e.ch}), blob: b });
            }
            e.cv.notify_all();
        }
    }
    for f in out {
        link.send(&f)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- per link: frames in

pub fn on_frame(node: &Node, link: &mut Link, f: &Frame) -> Result<(), LinkError> {
    if is_relayed(link) {
        return Ok(());
    }
    let from = link.peer_id.clone();
    let h = &f.head;
    let ch = h["ch"].as_u64().unwrap_or(0);
    match h["op"].as_str().unwrap_or("") {
        // ---- as the relay
        "hi" => {
            g().lock().unwrap().capable.insert(from);
        }
        "open" => open(node, &from, h),
        "accept" => {
            let mut st = g().lock().unwrap();
            let Some(c) = st.chans.get_mut(&ch).filter(|c| c.b == from && !c.accepted) else { return Ok(()) };
            c.accepted = true;
            let (a, b, n) = (c.a.clone(), c.b.clone(), c.n.take());
            st.to.entry(a.clone()).or_default().push_back(rl(json!({"op": "ok", "n": n, "ch": ch, "ip": ip_of(node, &b)})));
            let _ = node.app.event("mesh", &format!("relaying a link between {} and {}", short(&a), short(&b)));
        }
        "no" => {
            let mut st = g().lock().unwrap();
            if let Some(c) = st.chans.get(&ch).filter(|c| c.b == from && !c.accepted) {
                let (a, n) = (c.a.clone(), c.n.clone());
                st.chans.remove(&ch);
                let why = h["why"].as_str().unwrap_or("refused").chars().take(80).collect::<String>();
                st.to.entry(a).or_default().push_back(rl(json!({"op": "fail", "n": n, "why": why})));
            }
        }
        "d" => pass(node, &from, ch, &f.blob),
        "close" => {
            let mut st = g().lock().unwrap();
            if let Some(c) = st.chans.get(&ch).filter(|c| c.a == from || c.b == from) {
                let other = if c.a == from { c.b.clone() } else { c.a.clone() };
                close_chan(&mut st, ch, &other, "closed by the other side");
            }
        }
        // ---- as a user of the relay
        "offer" => {
            let mut st = g().lock().unwrap();
            if h["on"] == true {
                let ids: Vec<String> = h["ids"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).map(String::from)).take(1000).collect()).unwrap_or_default();
                st.relays.insert(from, ids);
            } else {
                st.relays.remove(&from);
            }
        }
        "in" => incoming(node, &from, h),
        "ok" => {
            let n = h["n"].as_str().unwrap_or("").to_string();
            let Some((relay, target, _)) = g().lock().unwrap().asking.remove(&n) else { return Ok(()) };
            if relay != from {
                return Ok(());
            }
            start_end(node, &from, ch, &target, h["ip"].as_str().unwrap_or(""), true);
        }
        "fail" => {
            let mut st = g().lock().unwrap();
            if let Some((_, target, _)) = h["n"].as_str().and_then(|n| st.asking.remove(n)) {
                st.cooling.insert(target.clone(), now_s() + COOL);
                let _ = node.app.event("mesh", &format!("relay {} can't link us to {}: {}", short(&from), short(&target), h["why"].as_str().unwrap_or("")));
            }
        }
        "rd" => {
            let e = g().lock().unwrap().ends.get(&(from, ch)).cloned();
            if let Some(e) = e {
                let mut p = e.p.lock().unwrap();
                if p.inbox.len() + f.blob.len() <= 4 * MAX_WAITING {
                    p.inbox.extend_from_slice(&f.blob);
                } else {
                    p.closed = Some("too much waiting to be read".into());
                }
                e.cv.notify_all();
            }
        }
        "end" => {
            let e = g().lock().unwrap().ends.get(&(from, ch)).cloned();
            if let Some(e) = e {
                e.p.lock().unwrap().closed = Some(format!("relay: {}", h["why"].as_str().unwrap_or("closed")));
                e.cv.notify_all();
            }
        }
        _ => {}
    }
    Ok(())
}

/// The relay: an arcade asks for a link to another of its links.
fn open(node: &Node, from: &str, h: &Value) {
    let n = h["n"].as_str().filter(|n| n.len() <= 32).unwrap_or("").to_string();
    let to = h["to"].as_str().unwrap_or("").to_string();
    let mut st = g().lock().unwrap();
    load_meter(node, &mut st.meter);
    st.meter.roll();
    let fail = |st: &mut G, why: &str| {
        st.to.entry(from.to_string()).or_default().push_back(rl(json!({"op": "fail", "n": n, "why": why})));
    };
    if !relay_on(node) {
        return fail(&mut st, "this arcade doesn't relay");
    }
    if over_limit(node, &st.meter) {
        return fail(&mut st, "this relay's monthly limit is reached");
    }
    if to == from || !st.capable.contains(&to) || !linked(node, &to) {
        return fail(&mut st, "not linked here");
    }
    if st.chans.len() as u64 >= max_links(node) {
        return fail(&mut st, "this relay is full");
    }
    if st.chans.values().filter(|c| c.a == from || c.b == from).count() >= MAX_OWN {
        return fail(&mut st, "too many relayed links for one arcade");
    }
    if st.chans.values().any(|c| (c.a == from && c.b == to) || (c.a == to && c.b == from)) {
        return fail(&mut st, "already relayed");
    }
    st.next += 1;
    let ch = st.next;
    st.chans.insert(ch, Chan { a: from.to_string(), b: to.clone(), n: Some(n), accepted: false, asked: Instant::now(), since: now_s(), bytes: 0 });
    st.to.entry(to).or_default().push_back(rl(json!({"op": "in", "ch": ch, "from": from, "ip": ip_of(node, from)})));
}

/// The relay: bytes of a link it passes, from one side to the other, counted against the month's limit.
fn pass(node: &Node, from: &str, ch: u64, bytes: &[u8]) {
    if bytes.is_empty() || bytes.len() > PIECE {
        return;
    }
    let mut st = g().lock().unwrap();
    let Some(c) = st.chans.get_mut(&ch).filter(|c| c.accepted && (c.a == from || c.b == from)) else { return };
    let other = if c.a == from { c.b.clone() } else { c.a.clone() };
    c.bytes += bytes.len() as u64;
    load_meter(node, &mut st.meter);
    st.meter.roll();
    st.meter.month_bytes += bytes.len() as u64;
    st.meter.day_bytes += bytes.len() as u64;
    if st.meter.saved.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
        save_meter(node, &mut st.meter);
    }
    if over_limit(node, &st.meter) || !relay_on(node) {
        let why = if relay_on(node) { "the relay's monthly limit is reached" } else { "the relay was turned off" };
        close_chan(&mut st, ch, &other, why);
        st.to.entry(from.to_string()).or_default().push_back(rl(json!({"op": "end", "ch": ch, "why": why})));
        return;
    }
    let w = st.waiting.entry((ch, other.clone())).or_default();
    *w += bytes.len();
    if *w > MAX_WAITING {
        close_chan(&mut st, ch, &other, "the other side isn't taking what is sent");
        st.to.entry(from.to_string()).or_default().push_back(rl(json!({"op": "end", "ch": ch, "why": "the other side isn't taking what is sent"})));
        return;
    }
    st.to.entry(other).or_default().push_back(Frame { head: json!({"t": "rl", "op": "rd", "ch": ch}), blob: bytes.to_vec() });
}

/// The relay: a link it passes ends; `tell` hears why.
fn close_chan(st: &mut G, ch: u64, tell: &str, why: &str) {
    if let Some(c) = st.chans.remove(&ch) {
        st.waiting.remove(&(ch, c.a.clone()));
        st.waiting.remove(&(ch, c.b.clone()));
        if let Some(n) = c.n.filter(|_| !c.accepted && tell == c.a) {
            st.to.entry(tell.to_string()).or_default().push_back(rl(json!({"op": "fail", "n": n, "why": why})));
        } else {
            st.to.entry(tell.to_string()).or_default().push_back(rl(json!({"op": "end", "ch": ch, "why": why})));
        }
    }
}

/// A link went down (its serve is over): as for `link_down`, unless it was itself relayed.
pub fn link_ended(link: &Link) {
    if !is_relayed(link) {
        link_down(&link.peer_id);
    }
}

/// A link to the relay `peer` went down: what passed through it ends, and so do our links through it.
fn link_down(peer: &str) {
    let mut st = g().lock().unwrap();
    st.said_hi.remove(peer);
    st.offered.remove(peer);
    st.capable.remove(peer);
    st.to.remove(peer);
    st.relays.remove(peer);
    st.ctl.remove(peer);
    let gone: Vec<(u64, String)> = st.chans.iter().filter(|(_, c)| c.a == peer || c.b == peer).map(|(ch, c)| (*ch, if c.a == peer { c.b.clone() } else { c.a.clone() })).collect();
    for (ch, other) in gone {
        close_chan(&mut st, ch, &other, "the other side's link to the relay went down");
    }
    for e in st.ends.values().filter(|e| e.relay == peer) {
        e.p.lock().unwrap().closed = Some("the link to the relay went down".into());
        e.cv.notify_all();
    }
}

// ---------------------------------------------------------------- as a user of relays

/// The relay brings a link from `from` (who asked it): take it if there is room and we aren't linked already.
fn incoming(node: &Node, relay: &str, h: &Value) {
    let ch = h["ch"].as_u64().unwrap_or(0);
    let from = h["from"].as_str().unwrap_or("").to_string();
    let mut st = g().lock().unwrap();
    // Only from an arcade that offered to relay (anyone else's "in" is dropped unanswered).
    if !st.relays.contains_key(relay) {
        return;
    }
    let mine = st.ends.len();
    let why = if from.len() != 64 || from == node.id_hex() {
        Some("not an arcade")
    } else if linked(node, &from) || st.up.contains_key(&from) {
        Some("already linked")
    } else if mine >= MAX_OWN {
        Some("too many relayed links here")
    } else {
        None
    };
    if let Some(why) = why {
        st.ctl.entry(relay.to_string()).or_default().push_back(rl(json!({"op": "no", "ch": ch, "why": why})));
        return;
    }
    st.ctl.entry(relay.to_string()).or_default().push_back(rl(json!({"op": "accept", "ch": ch})));
    drop(st);
    start_end(node, relay, ch, &from, h["ip"].as_str().unwrap_or(""), false);
}

/// Our side of a relayed link: the ordinary handshake over the pipe, then the link runs as any other.
fn start_end(node: &Node, relay: &str, ch: u64, peer: &str, ip: &str, dialer: bool) {
    let e = Arc::new(End { relay: relay.to_string(), ch, peer: peer.to_string(), p: Mutex::new(EndState::default()), cv: Condvar::new() });
    g().lock().unwrap().ends.insert((relay.to_string(), ch), e.clone());
    let (n, ip) = (node.self_arc(), ip.chars().filter(|c| c.is_ascii_hexdigit() || *c == '.' || *c == ':' || *c == '[' || *c == ']').take(64).collect::<String>());
    let _ = std::thread::Builder::new().name(format!("mesh relayed {}", short(peer))).spawn(move || run_end(n, e, ip, dialer));
}

fn run_end(node: Arc<Node>, e: Arc<End>, ip: String, dialer: bool) {
    let stream = RelayStream { e: e.clone(), buf: Vec::new(), timeout: Duration::from_secs(45) };
    let r = Link::handshake_over(Box::new(stream), crate::meshsvc::NET, &node.identity, dialer, node.mesh_port);
    let how = match r {
        Ok(mut link) if link.peer_id == e.peer && !linked(&node, &e.peer) => {
            let addr = format!("{ip}:{}", link.peer_listen.unwrap_or(0));
            g().lock().unwrap().up.insert(e.peer.clone(), e.relay.clone());
            let key = crate::node::link_add(&node.links, &addr, &e.peer, now_s());
            let _ = node.app.event("mesh", &format!("relayed link to {} through {}", short(&e.peer), short(&e.relay)));
            let r = crate::meshsvc::serve(&node, &mut link, &addr);
            crate::node::link_remove(&node.links, &key);
            format!("ended: {}", r.err().map(|e| e.to_string()).unwrap_or_default())
        }
        Ok(_) => "wrong arcade or already linked".to_string(),
        Err(err) => format!("handshake failed: {err}"),
    };
    let _ = node.app.event("mesh", &format!("relayed link to {} through {}: {how}", short(&e.peer), short(&e.relay)));
    let mut st = g().lock().unwrap();
    st.ends.remove(&(e.relay.clone(), e.ch));
    st.up.remove(&e.peer);
    st.cooling.insert(e.peer.clone(), now_s() + 120);
    if e.p.lock().unwrap().closed.is_none() {
        st.ctl.entry(e.relay.clone()).or_default().push_back(rl(json!({"op": "close", "ch": e.ch})));
    }
}

/// Every so often: ask our relays for links to arcades we can't otherwise reach.
pub fn round(node: &Node) {
    let mut st = g().lock().unwrap();
    if st.last_round.is_some_and(|t| t.elapsed() < ROUND_EVERY) {
        return;
    }
    st.last_round = Some(Instant::now());
    let now = now_s();
    st.cooling.retain(|_, until| *until > now);
    // Opens with no answer: given up.
    let stale: Vec<String> = st.asking.iter().filter(|(_, a)| a.2.elapsed() > OPEN_WAIT).map(|(n, _)| n.clone()).collect();
    for n in stale {
        if let Some((_, target, _)) = st.asking.remove(&n) {
            st.cooling.insert(target, now + COOL);
        }
    }
    // As the relay: opens the other side never answered.
    let unanswered: Vec<(u64, String)> = st.chans.iter().filter(|(_, c)| !c.accepted && c.asked.elapsed() > OPEN_WAIT).map(|(ch, c)| (*ch, c.a.clone())).collect();
    for (ch, a) in unanswered {
        close_chan(&mut st, ch, &a, "the other side didn't answer");
    }
    if st.meter.saved.is_some_and(|t| t.elapsed() > Duration::from_secs(60)) {
        save_meter(node, &mut st.meter);
    }
    let me = node.id_hex();
    let mut room = MAX_OWN.saturating_sub(st.ends.len() + st.asking.len());
    let relays: Vec<(String, Vec<String>)> = st.relays.iter().map(|(r, ids)| (r.clone(), ids.clone())).collect();
    for (relay, ids) in relays {
        for id in ids {
            if room == 0 {
                return;
            }
            // The lower id asks, so a pair opens one link.
            if id <= me || st.cooling.contains_key(&id) || st.up.contains_key(&id) || st.asking.values().any(|a| a.1 == id) || linked(node, &id) {
                continue;
            }
            let n = format!("{:016x}", rand_core::RngCore::next_u64(&mut rand_core::OsRng));
            st.asking.insert(n.clone(), (relay.clone(), id.clone(), Instant::now()));
            st.ctl.entry(relay.clone()).or_default().push_back(rl(json!({"op": "open", "n": n, "to": id})));
            room -= 1;
        }
    }
}

/// A relayed link's bytes, as the link handshake and frames read and write them (u32 BE length + body, as on TCP).
struct RelayStream {
    e: Arc<End>,
    buf: Vec<u8>,
    timeout: Duration,
}

impl RelayStream {
    fn whole(&self) -> io::Result<Option<usize>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let n = u32::from_be_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if n > arcade_mesh::transport::MAX_INBOUND {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        Ok((self.buf.len() >= 4 + n).then_some(n))
    }

    fn fill(&mut self, d: Duration) -> io::Result<bool> {
        let end = Instant::now() + d;
        let mut p = self.e.p.lock().unwrap();
        loop {
            if !p.inbox.is_empty() {
                self.buf.append(&mut p.inbox);
                return Ok(true);
            }
            if let Some(w) = &p.closed {
                return Err(io::Error::new(io::ErrorKind::ConnectionReset, w.clone()));
            }
            let now = Instant::now();
            if now >= end {
                return Ok(false);
            }
            p = self.e.cv.wait_timeout(p, end - now).unwrap().0;
        }
    }
}

impl arcade_mesh::transport::Transport for RelayStream {
    fn send(&mut self, body: &[u8]) -> io::Result<()> {
        if body.len() > arcade_mesh::transport::MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        let mut b = Vec::with_capacity(4 + body.len());
        b.extend_from_slice(&(body.len() as u32).to_be_bytes());
        b.extend_from_slice(body);
        let end = Instant::now() + Duration::from_secs(10);
        let mut p = self.e.p.lock().unwrap();
        // Wait while too much waits to go to the relay.
        while p.out_bytes + b.len() > MAX_OUT {
            if let Some(w) = &p.closed {
                return Err(io::Error::new(io::ErrorKind::ConnectionReset, w.clone()));
            }
            let now = Instant::now();
            if now >= end {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "relay not taking what is sent"));
            }
            p = self.e.cv.wait_timeout(p, end - now).unwrap().0;
        }
        if let Some(w) = &p.closed {
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, w.clone()));
        }
        for c in b.chunks(PIECE) {
            p.out_bytes += c.len();
            p.out.push_back(c.to_vec());
        }
        Ok(())
    }
    fn recv(&mut self) -> io::Result<Vec<u8>> {
        let end = Instant::now() + self.timeout;
        loop {
            if let Some(n) = self.whole()? {
                let body = self.buf[4..4 + n].to_vec();
                self.buf.drain(..4 + n);
                return Ok(body);
            }
            let now = Instant::now();
            if now >= end || !self.fill(end - now)? {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "relayed link read timed out"));
            }
        }
    }
    fn set_read_timeout(&mut self, d: Duration) -> io::Result<()> {
        self.timeout = d;
        Ok(())
    }
    fn describe(&self) -> String {
        format!("relay:{}", short(&self.e.relay))
    }
    fn wait_readable(&mut self, d: Duration) -> io::Result<bool> {
        if !self.buf.is_empty() {
            return Ok(true);
        }
        self.fill(d)
    }
}

// ---------------------------------------------------------------- the operator's view

/// `mesh.relay`: the relay switch, its limits, the meter, and the links it passes; and the links this arcade has
/// through relays.
pub fn status(node: &Node) -> Value {
    let mut st = g().lock().unwrap();
    load_meter(node, &mut st.meter);
    st.meter.roll();
    let gb = |b: u64| (b as f64 / 1e7).round() / 100.0;
    let tag = |id: &str| crate::meshview::tag_of(id).map(|t| format!("@{t}")).unwrap_or_else(|| short(id).to_string());
    json!({
        "on": relay_on(node),
        "limit_gb": limit_gb(node),
        "max_links": max_links(node),
        "month": st.meter.month,
        "used_gb": gb(st.meter.month_bytes),
        "today_gb": gb(st.meter.day_bytes),
        "over": over_limit(node, &st.meter),
        "passing": st.chans.values().filter(|c| c.accepted).map(|c| json!({"a": tag(&c.a), "b": tag(&c.b), "since": c.since, "mb": (c.bytes as f64 / 1e4).round() / 100.0})).collect::<Vec<_>>(),
        "takers": st.capable.len(),
        "through_relays": st.up.iter().map(|(p, r)| json!({"arcade": tag(p), "relay": tag(r)})).collect::<Vec<_>>(),
        "relays": st.relays.keys().map(|r| tag(r)).collect::<Vec<_>>(),
    })
}

/// `mesh.relay_set {on?, limit_gb?, max_links?}` (Admin).
pub fn set(node: &Node, req: &Value) -> Result<Value, String> {
    let mut c = cfg(node);
    if let Some(on) = req["on"].as_bool() {
        c["on"] = json!(on);
    }
    if let Some(l) = req.get("limit_gb").filter(|v| !v.is_null()) {
        let l = l.as_f64().filter(|l| *l >= 0.0 && *l <= 100_000.0).ok_or("limit_gb: a number of GB a month, 0 or more")?;
        c["limit_gb"] = json!(l);
    }
    if let Some(m) = req.get("max_links").filter(|v| !v.is_null()) {
        let m = m.as_u64().filter(|m| *m <= 1000).ok_or("max_links: a number of links, 0 to 1000")?;
        c["max_links"] = json!(m);
    }
    node.app.store.write(|w| w.put(arcade_app::META, b"mesh_relay", c.to_string().as_bytes())).map_err(|e| e.to_string())?;
    // Turned off: what it passes ends now.
    if c["on"] != true {
        let mut st = g().lock().unwrap();
        let all: Vec<(u64, String, String)> = st.chans.iter().map(|(ch, c)| (*ch, c.a.clone(), c.b.clone())).collect();
        for (ch, a, b) in all {
            close_chan(&mut st, ch, &a, "the relay was turned off");
            st.to.entry(b).or_default().push_back(rl(json!({"op": "end", "ch": ch, "why": "the relay was turned off"})));
        }
    }
    Ok(status(node))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_773), (2026, 11, 16));
        assert_eq!(civil(11_016), (2000, 2, 29));
    }

    /// Bytes written on one side of a relayed pipe come out whole on the other, in pieces no bigger than PIECE, and a
    /// closed pipe says so.
    #[test]
    fn a_relay_stream_carries_frames_whole_and_reports_its_end() {
        use arcade_mesh::transport::Transport;
        let e = Arc::new(End { relay: "r".repeat(64), ch: 1, peer: "p".repeat(64), p: Mutex::new(EndState::default()), cv: Condvar::new() });
        let mut s = RelayStream { e: e.clone(), buf: Vec::new(), timeout: Duration::from_millis(200) };
        let body = vec![7u8; 40_000];
        s.send(&body).unwrap();
        let pieces: Vec<Vec<u8>> = e.p.lock().unwrap().out.drain(..).collect();
        assert!(pieces.len() == 3 && pieces.iter().all(|p| p.len() <= PIECE));
        // What the other side gets is those pieces, in order.
        let mut r = RelayStream { e: Arc::new(End { relay: "r".repeat(64), ch: 1, peer: "q".repeat(64), p: Mutex::new(EndState::default()), cv: Condvar::new() }), buf: Vec::new(), timeout: Duration::from_millis(200) };
        for p in &pieces {
            r.e.p.lock().unwrap().inbox.extend_from_slice(p);
        }
        assert_eq!(r.recv().unwrap(), body);
        assert_eq!(r.recv().unwrap_err().kind(), io::ErrorKind::TimedOut);
        r.e.p.lock().unwrap().closed = Some("gone".into());
        assert_eq!(r.recv().unwrap_err().kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(r.describe(), "relay:rrrrrrrr");
    }
}
