//! **Shared models** (docs/LLM.md): an arcade whose operator runs a model server of their own (vLLM, llama.cpp,
//! Ollama, LM Studio: anything that speaks the OpenAI API) sells its replies to the mesh for ARCADE, and every arcade
//! buys from any seller through one OpenAI-compatible door of its own. Martin 2026-11-16 ("add open AI API to arcade V1
//! so that someone with a local model can share their inference to the network ... They should earn arcade coin"):
//! buyers pay sellers; nothing is minted.
//!
//! On the wire, frames `{"t":"lm", "op": ...}` (arcades without this ignore the kind):
//! - `offer {o, at, tag, name, pay_to, models:[{id, in, out}], at_once, max_out, refunds, busy, sig}`: a seller's offer,
//!   signed by its node key and passed on across the mesh (newer only); prices are ARCADE base units per million
//!   tokens. An offer with no models takes the seller off the list.
//! - `q {id, seq, end}` + a piece of the request (zlib JSON `{what, grant, ...}`), from a buyer's arcade to the seller
//!   over a mesh link (crvia). `what` = chat | speech | balance | paid | refund.
//! - `a {id, seq, end, err?, ev}`: the answers: for a chat each OpenAI stream chunk as it comes, then
//!   `{"arcade": {usage, cost, left}}`; for speech the audio in the frames' blobs (the first with `ev {type}`, keepalives
//!   `ev {wait}` while the voice is being made), then the same last event; for the others one answer.
//!
//! **Speech** (text to speech, OpenAI `/audio/speech`, e.g. the CSM voice server): a model whose server lists voices
//! (or names it tts/csm/speech) is offered with `kind: "speech"` and its voices; it is priced by `in` per million
//! characters of input, reserved in full before and charged only when audio came. A seller may set several model
//! servers (a chat server and a voice server), each with its own slots.
//!
//! **Paying**: credit is prepaid and kept by the seller's arcade, per buyer (the buyer's ARCADE address), for every
//! model of that seller. A buyer's arcade signs a grant naming the seller's node, its address and an expiry with that
//! address's key, so no other arcade can spend its credit. A top-up is an ARCADE payment to the seller's `pay_to` with
//! the memo `arcade-llm <buyer address>`; the seller credits it once it is in a block (never from the pool: a waiting
//! transaction can be replaced), once, to the address the memo names. A reply is charged by the seller's model server's
//! own count, reserved before (prompt guess + the longest reply the credit covers) and settled after; a reply that
//! errors costs nothing. Unused credit can be asked back unless the seller turned refunds off (shown on its offer).

use crate::node::Node;
use arcade_mesh::link::Frame;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, Read, Write};
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// One ARCADE in base units.
const COIN: u64 = arcade_arc::params::COIN;
/// Bytes of request in one `q` piece (well inside a link frame's 64 kB).
const PIECE: usize = 48 * 1024;
/// The largest request (inflated).
const MAX_REQUEST: usize = 8 << 20;
/// The longest a whole request may take.
const REQUEST_TIME: Duration = Duration::from_secs(600);
/// The longest between two answers of one request.
const GAP_TIME: Duration = Duration::from_secs(180);
/// How long a request waits for a free slot at the seller.
const SLOT_WAIT: Duration = Duration::from_secs(120);
/// Offers are said again this often, and dropped when older than OFFER_KEEP.
const OFFER_EVERY: Duration = Duration::from_secs(600);
const OFFER_KEEP: u64 = 1800;
/// A grant lasts a year.
const GRANT_SECS: u64 = 365 * 86400;
/// A payment not seen in a block within this is forgotten.
const CLAIM_WAIT: u64 = 6 * 3600;
/// The message magic grants are signed with (Core's signmessage, as handlegate's proofs).
const MAGIC: &str = arcade_wallet::keys::PEPECOIN_MAGIC;
/// OpenAI request fields passed on to the seller's model server; nothing else is.
const PASSED: &[&str] = &[
    "messages", "max_tokens", "max_completion_tokens", "temperature", "top_p", "top_k", "min_p", "stop", "tools", "tool_choice",
    "parallel_tool_calls", "response_format", "seed", "presence_penalty", "frequency_penalty", "repetition_penalty", "logit_bias",
    "n", "user", "reasoning_effort", "chat_template_kwargs",
];
/// OpenAI speech request fields passed on to the voice server.
const SPEECH_PASSED: &[&str] = &["input", "voice", "response_format", "speed", "instructions"];
/// The longest speech input (OpenAI's own limit).
const MAX_SPEECH_CHARS: u64 = 4096;
/// The most audio one speech request brings back.
const MAX_AUDIO: usize = 64 << 20;
/// The longest a speech request may take (a voice server may make all the audio before it sends any).
const SPEECH_TIME: Duration = Duration::from_secs(1200);
/// Usage rows kept by a seller.
const USAGE_KEEP: usize = 2000;

fn now_s() -> u64 {
    arcade_app::now()
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

fn coins(units: u64) -> f64 {
    units as f64 / COIN as f64
}

fn to_units(arcade: f64) -> u64 {
    if arcade.is_finite() && arcade > 0.0 { (arcade * COIN as f64).round() as u64 } else { 0 }
}

pub fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ---------------------------------------------------------------- settings

/// The seller's settings (META "llm"): {on, servers:[{url, key}], models:[id], prices:{id:{in, out}} (ARCADE per
/// million tokens; for a speech model `in` is per million characters), info:{id:{server, kind, voices}} (from the last
/// scan), at_once (per server), max_out, refunds, own_free}. Before speech, one server was `url` + `key`; still read.
fn cfg(node: &Node) -> Value {
    read_meta(node, b"llm").unwrap_or(json!({}))
}

fn read_meta(node: &Node, k: &[u8]) -> Option<Value> {
    node.app.store.read(|r| r.get(arcade_app::META, k)).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok())
}

fn write_meta(node: &Node, k: &[u8], v: &Value) -> Result<(), String> {
    node.app.store.write(|w| w.put(arcade_app::META, k, v.to_string().as_bytes())).map_err(err)
}

/// A model server's address as kept: http(s), no trailing slash, ending in /v1.
fn norm_url(u: &str) -> Option<String> {
    let u = u.trim().trim_end_matches('/');
    if !(u.starts_with("http://") || u.starts_with("https://")) || u.len() > 300 {
        return None;
    }
    Some(if u.ends_with("/v1") { u.to_string() } else { format!("{u}/v1") })
}

/// The model servers set (url, key), in order.
fn servers(c: &Value) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = match c["servers"].as_array() {
        Some(a) => a.iter().filter_map(|s| Some((norm_url(s["url"].as_str()?)?, s["key"].as_str().unwrap_or("").to_string()))).collect(),
        None => c["url"].as_str().and_then(norm_url).map(|u| vec![(u, c["key"].as_str().unwrap_or("").to_string())]).unwrap_or_default(),
    };
    v.dedup_by(|a, b| a.0 == b.0);
    v
}

fn selling(c: &Value) -> bool {
    c["on"] == true && !servers(c).is_empty() && c["models"].as_array().is_some_and(|m| !m.is_empty())
}

fn at_once(c: &Value) -> usize {
    c["at_once"].as_u64().unwrap_or(2).clamp(1, 32) as usize
}

fn max_out(c: &Value) -> u64 {
    c["max_out"].as_u64().unwrap_or(4096).clamp(16, 131_072)
}

/// A model's prices here: (in, out) in base units per million tokens (characters in, for speech).
fn price_of(c: &Value, model: &str) -> (u64, u64) {
    let p = &c["prices"][model];
    (to_units(p["in"].as_f64().unwrap_or(0.0)), to_units(p["out"].as_f64().unwrap_or(0.0)))
}

/// "speech" for a text-to-speech model (OpenAI /audio/speech), else "chat".
fn kind_of(c: &Value, model: &str) -> &'static str {
    if c["info"][model]["kind"] == "speech" { "speech" } else { "chat" }
}

/// What a model server's model-list entry is: a speech model lists its voices (CSM voice server), or says so by name.
fn kind_in_list(m: &Value) -> &'static str {
    let id = m["id"].as_str().unwrap_or("").to_lowercase();
    if m["voices"].is_array() || ["tts", "csm", "speech", "kokoro", "orpheus"].iter().any(|w| id.contains(w)) { "speech" } else { "chat" }
}

/// What `tin` tokens in and `tout` out cost at (in, out), in thousandths of a base unit.
fn cost_mu(price: (u64, u64), tin: u64, tout: u64) -> u64 {
    ((tin as u128 * price.0 as u128 + tout as u128 * price.1 as u128) / 1000).min(u64::MAX as u128) as u64
}

/// The most tokens out `mu` covers after `tin` in, at (in, out) (None: replies cost nothing).
fn affordable_out(price: (u64, u64), mu: u64, tin: u64) -> Option<u64> {
    if price.1 == 0 {
        return if cost_mu(price, tin, 0) <= mu { None } else { Some(0) };
    }
    let left = mu.saturating_sub(cost_mu(price, tin, 0)) as u128;
    Some((left * 1000 / price.1 as u128).min(u64::MAX as u128) as u64)
}

/// The model server that has `model` (always one the operator set; never a URL from a request): the one the last
/// scan found it on, else the first.
fn server_for(c: &Value, model: &str) -> Result<(String, String), String> {
    let all = servers(c);
    let at = c["info"][model]["server"].as_str().unwrap_or("");
    all.iter().find(|s| s.0 == at).or(all.first()).cloned().ok_or_else(|| "set the model server's address (http://host:port/v1)".to_string())
}

fn agent(secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout_read(Duration::from_secs(secs)).user_agent("arcade-llm/1.0").build()
}

/// The models one server says it has.
fn scan_one(url: &str, key: &str) -> Result<Vec<Value>, String> {
    let mut r = agent(15).get(&format!("{url}/models"));
    if !key.is_empty() {
        r = r.set("Authorization", &format!("Bearer {key}"));
    }
    let v: Value = r.call().map_err(|e| format!("{url} did not answer: {e}"))?.into_json().map_err(|_| format!("{url}'s model list isn't JSON"))?;
    Ok(v["data"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|m| {
        let id = m["id"].as_str()?.to_string();
        let kind = kind_in_list(&m);
        let mut x = json!({"id": id, "ctx": m["max_model_len"].as_u64().or(m["context_length"].as_u64()), "server": url, "kind": kind});
        if let Some(vs) = m["voices"].as_array() {
            x["voices"] = json!(vs.iter().filter_map(|s| s.as_str()).take(64).map(|s| s.chars().take(60).collect::<String>()).collect::<Vec<_>>());
        }
        Some(x)
    }).collect())
}

/// The models every server set says it has (an error only when none answered), and what went wrong with the others.
fn scan_models(c: &Value) -> Result<(Vec<Value>, Vec<String>), String> {
    let all = servers(c);
    if all.is_empty() {
        return Err("set the model server's address (http://host:port/v1)".into());
    }
    let (mut found, mut bad) = (Vec::new(), Vec::new());
    for (url, key) in &all {
        match scan_one(url, key) {
            Ok(m) => found.extend(m),
            Err(e) => bad.push(e),
        }
    }
    if found.is_empty() && !bad.is_empty() {
        return Err(bad.join("; "));
    }
    Ok((found, bad))
}

/// Keep where each model lives and what it is, from a scan (models not found now keep what was known).
fn note_info(c: &mut Value, found: &[Value]) {
    let mut info = c["info"].as_object().cloned().unwrap_or_default();
    for m in found {
        if let Some(id) = m["id"].as_str() {
            info.insert(id.to_string(), json!({"server": m["server"], "kind": m["kind"], "voices": m["voices"]}));
        }
    }
    let shared: BTreeSet<String> = c["models"].as_array().into_iter().flatten().filter_map(|m| m.as_str().map(String::from)).collect();
    let live: BTreeSet<String> = found.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect();
    info.retain(|k, _| shared.contains(k) || live.contains(k));
    c["info"] = Value::Object(info);
}

/// `llm.status`: the selling settings, what was earned, and the models the servers have now.
pub fn status(node: &Node) -> Value {
    let c = cfg(node);
    let all = servers(&c);
    let mut out = json!({
        "on": c["on"] == true, "servers": all.iter().map(|s| json!({"url": s.0, "has_key": !s.1.is_empty()})).collect::<Vec<_>>(),
        "url": all.first().map(|s| s.0.as_str()).unwrap_or(""), "has_key": all.first().is_some_and(|s| !s.1.is_empty()),
        "models": c["models"].clone(), "prices": c["prices"].clone(), "info": c["info"].clone(), "at_once": at_once(&c), "max_out": max_out(&c),
        "refunds": c["refunds"] != false, "own_free": c["own_free"] != false, "selling": selling(&c),
        "pay_to": crate::arcledger::own_address(node).ok(), "busy": busy_all(),
    });
    if !all.is_empty() {
        match scan_models(&c) {
            Ok((m, bad)) => {
                out["found"] = json!(m);
                if !bad.is_empty() {
                    out["found_error"] = json!(bad.join("; "));
                }
            }
            Err(e) => out["found_error"] = json!(e),
        }
    }
    let b = books().lock().unwrap();
    let mut b2 = b.clone();
    drop(b);
    load_books(node, &mut b2);
    let day = now_s().saturating_sub(86400);
    let (mut n, mut tin, mut tout, mut mu, mut n_day, mut mu_day) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    for u in &b2.usage {
        n += 1;
        tin += u.tin;
        tout += u.tout;
        mu += u.mu;
        if u.at >= day {
            n_day += 1;
            mu_day += u.mu;
        }
    }
    let held: u64 = b2.credit.values().sum();
    out["earned"] = json!({"requests": n, "tokens_in": tin, "tokens_out": tout, "arcade": coins(mu / 1000), "day_requests": n_day, "day_arcade": coins(mu_day / 1000),
                           "paid_in": coins(b2.paid_in), "credit_held": coins(held / 1000), "buyers": b2.credit.len(), "waiting_payments": b2.claims.len()});
    out
}

/// The servers a request names: `servers:[{url, key?}]`, or the one `url` + `key?` (as before speech). A key left
/// out keeps the one kept for that address.
fn servers_asked(c: &Value, req: &Value) -> Result<Option<Vec<Value>>, String> {
    let asked: Vec<(String, Option<String>)> = if let Some(a) = req["servers"].as_array() {
        a.iter().take(8).map(|s| (s["url"].as_str().unwrap_or("").to_string(), s["key"].as_str().map(String::from))).collect()
    } else if let Some(u) = req["url"].as_str() {
        vec![(u.to_string(), req["key"].as_str().map(String::from))]
    } else if let Some(k) = req["key"].as_str() {
        // Only a new key: for the first server.
        let mut all = servers(c);
        if let Some(s) = all.first_mut() {
            s.1 = k.trim().to_string();
        }
        return Ok(Some(all.into_iter().map(|(u, k)| json!({"url": u, "key": k})).collect()));
    } else {
        return Ok(None);
    };
    let had = servers(c);
    let mut out = Vec::new();
    for (u, k) in asked {
        if u.trim().is_empty() {
            continue;
        }
        let url = norm_url(&u).ok_or_else(|| format!("{}: a model server's address starts with http:// or https://", u.trim()))?;
        let key = match k.map(|k| k.trim().to_string()).filter(|k| !k.is_empty()) {
            Some(k) => k,
            None => had.iter().find(|s| s.0 == url).map(|s| s.1.clone()).unwrap_or_default(),
        };
        if !out.iter().any(|x: &Value| x["url"] == url.as_str()) {
            out.push(json!({"url": url, "key": key}));
        }
    }
    Ok(Some(out))
}

/// `llm.set`: change the selling settings (only the fields given).
pub fn set(node: &Node, req: &Value) -> Result<Value, String> {
    let mut c = cfg(node);
    if !c.is_object() {
        c = json!({});
    }
    if let Some(s) = servers_asked(&c, req)? {
        c["servers"] = json!(s);
        c.as_object_mut().map(|o| {
            o.remove("url");
            o.remove("key");
        });
    }
    for k in ["on", "refunds", "own_free"] {
        if let Some(b) = req[k].as_bool() {
            c[k] = json!(b);
        }
    }
    if let Some(n) = req["at_once"].as_u64() {
        c["at_once"] = json!(n.clamp(1, 32));
    }
    if let Some(n) = req["max_out"].as_u64() {
        c["max_out"] = json!(n.clamp(16, 131_072));
    }
    if let Some(m) = req["models"].as_array() {
        let ids: Vec<String> = m.iter().filter_map(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s.len() <= 200).take(32).collect();
        c["models"] = json!(ids);
    }
    if let Some(p) = req["prices"].as_object() {
        let mut out = Map::new();
        for (m, v) in p.iter().take(64) {
            let get = |k: &str| v[k].as_f64().or_else(|| v[k].as_str().and_then(|s| s.trim().parse().ok())).unwrap_or(0.0);
            let (i, o) = (get("in"), get("out"));
            if !(0.0..=1_000_000.0).contains(&i) || !(0.0..=1_000_000.0).contains(&o) {
                return Err(format!("{m}: a price is ARCADE per million tokens (characters, for speech), 0 to 1000000"));
            }
            out.insert(m.clone(), json!({"in": i, "out": o}));
        }
        c["prices"] = Value::Object(out);
    }
    if c["on"] == true && servers(&c).is_empty() {
        return Err("set the model server's address (http://host:port/v1)".into());
    }
    if let Ok((found, _)) = scan_models(&c) {
        note_info(&mut c, &found);
    }
    write_meta(node, b"llm", &c)?;
    say_offer(node);
    Ok(status(node))
}

/// `llm.scan {servers? | url?, key?}`: the models the servers have (the ones set, or the ones given, before saving).
pub fn scan(node: &Node, req: &Value) -> Result<Value, String> {
    let mut c = cfg(node);
    if let Some(s) = servers_asked(&c, req)? {
        c["servers"] = json!(s);
    }
    let (models, bad) = scan_models(&c)?;
    Ok(json!({"models": models, "errors": bad}))
}

// ---------------------------------------------------------------- the seller's books

#[derive(Clone, Default, Serialize, Deserialize)]
struct Use {
    at: u64,
    who: String,
    model: String,
    tin: u64,
    tout: u64,
    mu: u64,
    ms: u64,
    ok: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Books {
    /// buyer address -> credit, thousandths of a base unit
    credit: BTreeMap<String, u64>,
    /// txids credited (or refunds paid)
    claimed: BTreeSet<String>,
    /// payments said, waiting for a block: (buyer address, txid, when said)
    claims: Vec<(String, String, u64)>,
    usage: Vec<Use>,
    /// all top-ups credited, base units
    paid_in: u64,
    #[serde(skip)]
    loaded: bool,
    #[serde(skip)]
    dirty: bool,
}

fn books() -> &'static Mutex<Books> {
    static B: LazyLock<Mutex<Books>> = LazyLock::new(Default::default);
    &B
}

fn books_file(node: &Node) -> std::path::PathBuf {
    node.home.join("llm-books.json")
}

fn load_books(node: &Node, b: &mut Books) {
    if b.loaded {
        return;
    }
    if let Some(saved) = std::fs::read_to_string(books_file(node)).ok().and_then(|t| serde_json::from_str::<Books>(&t).ok()) {
        *b = saved;
    }
    b.loaded = true;
}

fn save_books(node: &Node, b: &mut Books) {
    if let Ok(t) = serde_json::to_string(b) {
        let tmp = node.home.join("llm-books.json.new");
        if std::fs::write(&tmp, t).is_ok() {
            let _ = std::fs::rename(&tmp, books_file(node));
        }
    }
    b.dirty = false;
}

fn with_books<T>(node: &Node, f: impl FnOnce(&mut Books) -> T) -> T {
    let mut b = books().lock().unwrap();
    load_books(node, &mut b);
    let out = f(&mut b);
    if b.dirty {
        save_books(node, &mut b);
    }
    out
}

/// The memo a top-up for `address` carries.
fn memo_of(address: &str) -> Vec<u8> {
    format!("arcade-llm {}", &address[..address.len().min(64)]).into_bytes()
}

/// Payments said: credited once in a block, to the address their memo names, by what they pay this arcade.
fn settle_claims(node: &Node) {
    let Ok(p) = crate::arcledger::chain() else { return };
    let Some(to) = crate::arcledger::own_address(node).ok().and_then(|a| arcade_ledger::address::script_for_address(p, &a)) else { return };
    with_books(node, |b| {
        if b.claims.is_empty() {
            return;
        }
        let now = now_s();
        for (who, txid, at) in std::mem::take(&mut b.claims) {
            let Ok(id) = arcade_chain::hash::Hash256::from_hex(&txid) else { continue };
            match crate::relaysvc::confirmed_tx(node, &id) {
                Some(tx) => {
                    let paid: u64 = tx.outputs.iter().filter(|o| o.script_pubkey == to).map(|o| o.value).sum();
                    let memo = memo_of(&who);
                    if paid > 0 && tx.raw.windows(memo.len()).any(|w| w == memo.as_slice()) && b.claimed.insert(txid.clone()) {
                        *b.credit.entry(who.clone()).or_default() += paid.saturating_mul(1000);
                        b.paid_in += paid;
                        let _ = node.app.event("llm", &format!("{} bought {} ARCADE of model credit ({txid})", short(&who), coins(paid)));
                    }
                    b.dirty = true;
                }
                None if now.saturating_sub(at) < CLAIM_WAIT => b.claims.push((who, txid, at)),
                None => b.dirty = true,
            }
        }
    });
}

// ---------------------------------------------------------------- grants

fn grant_words(seller: &str, address: &str, expires: u64) -> String {
    format!("arcade-llm grant {seller} {address} {expires}")
}

/// The address a grant proves for this seller (`me`), or why not.
fn grant_ok(g: &Value, me: &str) -> Result<String, String> {
    let address = g["address"].as_str().filter(|a| (20..=64).contains(&a.len())).ok_or("no grant")?;
    let expires = g["expires"].as_u64().unwrap_or(0);
    let now = now_s();
    if expires < now {
        return Err("that grant has run out".into());
    }
    if expires > now + GRANT_SECS + 3600 {
        return Err("a grant lasts a year at most".into());
    }
    let sig = g["sig"].as_str().unwrap_or("");
    if !arcade_wallet::keys::verifymessage(address, sig, MAGIC, &grant_words(me, address, expires)) {
        return Err("that grant's signature does not check".into());
    }
    Ok(address.to_string())
}

/// This wallet's grant for `seller` (made once, kept a year).
fn my_grant(node: &Node, seller: &str) -> Result<Value, String> {
    let p = crate::arcledger::chain()?;
    let (address, _, sk) = crate::arcledger::sender(node, p)?;
    let key = format!("llm_grant:{seller}");
    if let Some(g) = read_meta(node, key.as_bytes()) {
        if g["address"] == address.as_str() && g["expires"].as_u64().unwrap_or(0) > now_s() + 30 * 86400 {
            return Ok(g);
        }
    }
    let expires = now_s() + GRANT_SECS;
    let sig = arcade_wallet::keys::signmessage(&sk, MAGIC, &grant_words(seller, &address, expires));
    let g = json!({"address": address, "expires": expires, "sig": sig});
    write_meta(node, key.as_bytes(), &g)?;
    Ok(g)
}

// ---------------------------------------------------------------- offers

struct Seen {
    offers: HashMap<String, Value>,
    last_said: Option<Instant>,
}

fn seen() -> &'static Mutex<Seen> {
    static S: LazyLock<Mutex<Seen>> = LazyLock::new(|| Mutex::new(Seen { offers: HashMap::new(), last_said: None }));
    &S
}

/// This arcade's own offer now (signed), or one with no models while it sells nothing.
fn own_offer(node: &Node) -> Value {
    let c = cfg(node);
    let models: Vec<Value> = if selling(&c) {
        c["models"].as_array().cloned().unwrap_or_default().iter().filter_map(|m| m.as_str()).map(|m| {
            let (i, o) = price_of(&c, m);
            if kind_of(&c, m) == "speech" {
                json!({"id": m, "in": i, "out": 0, "kind": "speech", "voices": c["info"][m]["voices"]})
            } else {
                json!({"id": m, "in": i, "out": o})
            }
        }).collect()
    } else {
        Vec::new()
    };
    let tag = crate::arcledger::chain().ok().and_then(|p| crate::arcledger::sender(node, p).ok()).and_then(|s| s.1);
    let mut o = json!({
        "t": "lm", "op": "offer", "o": node.id_hex(), "at": now_s(), "tag": tag, "name": crate::meshsvc::public_name(node),
        "pay_to": crate::arcledger::own_address(node).ok(), "models": models, "at_once": at_once(&c), "max_out": max_out(&c),
        "refunds": c["refunds"] != false, "busy": busy_all(),
    });
    arcade_mesh::canon::sign_json(&node.identity, &mut o, &["sig"], b"");
    o
}

/// Say our offer to the mesh (now: settings changed), if we sell or sold.
fn say_offer(node: &Node) {
    let o = own_offer(node);
    let mut s = seen().lock().unwrap();
    let had = s.offers.get(&node.id_hex()).is_some_and(|x| x["models"].as_array().is_some_and(|m| !m.is_empty()));
    let sells = o["models"].as_array().is_some_and(|m| !m.is_empty());
    if sells || had {
        s.offers.insert(node.id_hex(), o.clone());
        s.last_said = Some(Instant::now());
        drop(s);
        node.mesh_outbox.lock().unwrap().push(o);
    }
}

/// Another arcade's offer, from a link or passed on: kept and passed on when signed and newer.
fn on_offer(node: &Node, h: &Value) {
    let Some(o) = h["o"].as_str().filter(|o| crate::udpsvc::is_hex(o, 64)) else { return };
    if o == node.id_hex() || !arcade_mesh::canon::verify_json(h, &["sig"], "o", b"") {
        return;
    }
    let at = h["at"].as_u64().unwrap_or(0);
    let now = now_s();
    if at > now + 600 || at + OFFER_KEEP < now {
        return;
    }
    let mut s = seen().lock().unwrap();
    if s.offers.get(o).and_then(|x| x["at"].as_u64()).is_some_and(|had| had >= at) {
        return;
    }
    s.offers.insert(o.to_string(), h.clone());
    drop(s);
    node.mesh_outbox.lock().unwrap().push(h.clone());
}

/// Offers now, fresh and selling, ours first.
fn offers(node: &Node) -> Vec<Value> {
    let now = now_s();
    let me = node.id_hex();
    let s = seen().lock().unwrap();
    let mut v: Vec<Value> = s.offers.values().filter(|o| o["o"] == me.as_str() || o["at"].as_u64().unwrap_or(0) + OFFER_KEEP >= now)
        .filter(|o| o["models"].as_array().is_some_and(|m| !m.is_empty())).cloned().collect();
    drop(s);
    v.sort_by_key(|o| (o["o"] != me.as_str(), o["tag"].as_str().unwrap_or("~").to_string()));
    v
}

fn offer_of(node: &Node, seller: &str) -> Option<Value> {
    if seller == node.id_hex() {
        let o = own_offer(node);
        return o["models"].as_array().is_some_and(|m| !m.is_empty()).then_some(o);
    }
    offers(node).into_iter().find(|o| o["o"] == seller)
}

/// `llm.offers`: every model on offer, with our own record of each seller.
pub fn list_offers(node: &Node) -> Value {
    let me = node.id_hex();
    let rec = records_loaded(node);
    let out: Vec<Value> = offers(node).into_iter().map(|o| {
        let id = o["o"].as_str().unwrap_or("").to_string();
        let models: Vec<Value> = o["models"].as_array().cloned().unwrap_or_default().into_iter().map(|m| {
            json!({"id": m["id"], "in": coins(m["in"].as_u64().unwrap_or(0)), "out": coins(m["out"].as_u64().unwrap_or(0)),
                   "free": m["in"].as_u64().unwrap_or(0) == 0 && m["out"].as_u64().unwrap_or(0) == 0,
                   "kind": if m["kind"] == "speech" { "speech" } else { "chat" }, "voices": m["voices"]})
        }).collect();
        json!({"seller": id, "here": id == me, "tag": o["tag"], "name": o["name"], "models": models, "max_out": o["max_out"], "at_once": o["at_once"],
               "refunds": o["refunds"], "busy": o["busy"], "at": o["at"], "linked": id == me || crate::crvia::linked(&id),
               "record": rec.sellers.get(&id).cloned(), "credit": rec.credit.get(&id).map(|c| coins(*c / 1000))})
    }).collect();
    json!(out)
}

// ---------------------------------------------------------------- the buyer's record of sellers

#[derive(Clone, Default, Serialize, Deserialize)]
struct Record {
    requests: u64,
    failed: u64,
    /// tokens out the sellers billed, and what we counted ourselves
    billed_out: u64,
    counted_out: u64,
    first_ms: u64,
    out_ms: u64,
    spent_mu: u64,
    paid: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Records {
    sellers: BTreeMap<String, Record>,
    /// seller -> last credit heard, thousandths of a base unit
    credit: BTreeMap<String, u64>,
    /// day -> auto top-ups paid that day (base units)
    auto_day: (String, u64),
    #[serde(skip)]
    loaded: bool,
}

fn records() -> &'static Mutex<Records> {
    static R: LazyLock<Mutex<Records>> = LazyLock::new(Default::default);
    &R
}

/// The records, loaded from disk the first time (after a restart, before any request).
fn records_loaded(node: &Node) -> std::sync::MutexGuard<'static, Records> {
    let mut r = records().lock().unwrap();
    if !r.loaded {
        if let Some(saved) = std::fs::read_to_string(node.home.join("llm-sellers.json")).ok().and_then(|t| serde_json::from_str::<Records>(&t).ok()) {
            *r = saved;
        }
        r.loaded = true;
    }
    r
}

fn with_records<T>(node: &Node, f: impl FnOnce(&mut Records) -> T) -> T {
    let path = node.home.join("llm-sellers.json");
    let mut r = records_loaded(node);
    let out = f(&mut r);
    if let Ok(t) = serde_json::to_string(&*r) {
        let tmp = node.home.join("llm-sellers.json.new");
        if std::fs::write(&tmp, t).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    out
}

// ---------------------------------------------------------------- slots at the seller

struct Slots {
    busy: Mutex<usize>,
    cv: Condvar,
}

impl Slots {
    fn busy(&self) -> usize {
        *self.busy.lock().unwrap()
    }
    fn take(&self, most: usize, wait: Duration) -> bool {
        let end = Instant::now() + wait;
        let mut b = self.busy.lock().unwrap();
        while *b >= most {
            let now = Instant::now();
            if now >= end {
                return false;
            }
            b = self.cv.wait_timeout(b, end - now).unwrap().0;
        }
        *b += 1;
        true
    }
    fn give(&self) {
        *self.busy.lock().unwrap() -= 1;
        self.cv.notify_one();
    }
}

fn slot_map() -> &'static Mutex<HashMap<String, std::sync::Arc<Slots>>> {
    static S: LazyLock<Mutex<HashMap<String, std::sync::Arc<Slots>>>> = LazyLock::new(Default::default);
    &S
}

/// One set of slots per model server (a voice server and a chat server don't wait for each other).
fn slots(url: &str) -> std::sync::Arc<Slots> {
    slot_map().lock().unwrap().entry(url.to_string()).or_insert_with(|| std::sync::Arc::new(Slots { busy: Mutex::new(0), cv: Condvar::new() })).clone()
}

/// Requests running now, on every server.
fn busy_all() -> usize {
    slot_map().lock().unwrap().values().map(|s| s.busy()).sum()
}

// ---------------------------------------------------------------- serving a chat (the seller's side)

fn text_len(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().map(text_len).sum(),
        Value::Object(o) => o.iter().filter(|(k, _)| *k != "image_url").map(|(_, x)| text_len(x)).sum(),
        _ => 0,
    }
}

/// A rough count of the prompt's tokens (to reserve before the server counts).
fn guess_tokens(body: &Value) -> u64 {
    (text_len(&body["messages"]) + text_len(&body["tools"])) as u64 / 3 + 16
}

/// The body for the model server: the passed fields only, the model, stream with usage, and the longest reply capped.
fn server_body(body: &Value, model: &str, cap: u64) -> Value {
    let mut out = Map::new();
    for k in PASSED {
        if let Some(v) = body.get(*k) {
            out.insert((*k).to_string(), v.clone());
        }
    }
    out.remove("max_completion_tokens");
    let asked = body["max_completion_tokens"].as_u64().or(body["max_tokens"].as_u64()).unwrap_or(cap);
    out.insert("max_tokens".into(), json!(asked.min(cap).max(1)));
    out.insert("model".into(), json!(model));
    out.insert("stream".into(), json!(true));
    out.insert("stream_options".into(), json!({"include_usage": true}));
    out.insert("n".into(), json!(1));
    Value::Object(out)
}

/// Run one chat on our own model server: each stream chunk to `each`; the usage (tokens in, out) it reported.
fn run_server((url, key): &(String, String), body: &Value, each: &mut dyn FnMut(Value) -> Result<(), String>) -> Result<(u64, u64, u64), String> {
    let mut r = agent(GAP_TIME.as_secs()).post(&format!("{url}/chat/completions")).set("Content-Type", "application/json");
    if !key.is_empty() {
        r = r.set("Authorization", &format!("Bearer {key}"));
    }
    let started = Instant::now();
    let resp = match r.send_string(&body.to_string()) {
        Ok(x) => x,
        Err(ureq::Error::Status(code, x)) => {
            let t = x.into_string().unwrap_or_default();
            let said = serde_json::from_str::<Value>(&t).ok().and_then(|v| v["error"]["message"].as_str().or(v["message"].as_str()).map(String::from)).unwrap_or(t);
            return Err(format!("the model server said {code}: {}", said.chars().take(300).collect::<String>()));
        }
        Err(e) => return Err(format!("the model server did not answer: {e}")),
    };
    let mut usage = (0u64, 0u64);
    let mut first_ms = 0u64;
    let mut rd = std::io::BufReader::new(resp.into_reader().take(256 << 20));
    let mut line = String::new();
    loop {
        line.clear();
        if rd.read_line(&mut line).map_err(|e| format!("the model server's answer broke off: {e}"))? == 0 {
            break;
        }
        let Some(data) = line.trim().strip_prefix("data:").map(str::trim) else { continue };
        if data == "[DONE]" {
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else { continue };
        if chunk["usage"].is_object() {
            usage = (chunk["usage"]["prompt_tokens"].as_u64().unwrap_or(0), chunk["usage"]["completion_tokens"].as_u64().unwrap_or(0));
        }
        if chunk["error"].is_object() {
            return Err(format!("the model server said: {}", chunk["error"]["message"].as_str().unwrap_or("an error")));
        }
        if first_ms == 0 && chunk["choices"].as_array().is_some_and(|a| !a.is_empty()) {
            first_ms = started.elapsed().as_millis() as u64;
        }
        each(chunk)?;
    }
    Ok((usage.0, usage.1, first_ms))
}

/// A chat for `who` (an address, or None: this arcade's own operator): reserve, run, settle. Chunks go to `each`; the
/// last event ({"arcade": ...}) is returned.
fn serve_chat(node: &Node, who: Option<&str>, body: &Value, each: &mut dyn FnMut(Value) -> Result<(), String>) -> Result<Value, String> {
    let c = cfg(node);
    if !selling(&c) && who.is_some() {
        return Err("this arcade isn't sharing a model now".into());
    }
    let models: Vec<String> = c["models"].as_array().cloned().unwrap_or_default().iter().filter_map(|m| m.as_str().map(String::from)).collect();
    let model = body["model"].as_str().map(String::from).filter(|m| !m.is_empty()).or_else(|| models.first().cloned()).ok_or("no model is shared here")?;
    if who.is_some() && !models.contains(&model) {
        return Err(format!("{model} isn't shared here (these are: {})", models.join(", ")));
    }
    if kind_of(&c, &model) == "speech" {
        return Err(format!("{model} is a speech model: ask it through audio/speech"));
    }
    let srv = server_for(&c, &model)?;
    if !body["messages"].as_array().is_some_and(|m| !m.is_empty() && m.len() <= 2000) {
        return Err("messages: 1 to 2000 of them".into());
    }
    let free_own = who.is_none() && c["own_free"] != false;
    let price = if free_own { (0, 0) } else { price_of(&c, &model) };
    let guess = guess_tokens(body);
    let mut cap = max_out(&c);
    let mut reserved = 0u64;
    if let Some(w) = who {
        if price != (0, 0) {
            // Reserve what the longest reply the credit covers would cost.
            let r = with_books(node, |b| {
                let have = b.credit.get(w).copied().unwrap_or(0);
                match affordable_out(price, have, guess) {
                    Some(0) => Err(format!("not enough credit: {} ARCADE left here", coins(have / 1000))),
                    Some(n) => {
                        let out = n.min(cap);
                        let take = cost_mu(price, guess, out).min(have);
                        *b.credit.get_mut(w).unwrap() -= take;
                        b.dirty = true;
                        Ok((out, take))
                    }
                    None => {
                        let take = cost_mu(price, guess, 0).min(have);
                        *b.credit.entry(w.to_string()).or_default() -= take;
                        b.dirty = true;
                        Ok((cap, take))
                    }
                }
            })?;
            cap = r.0.max(1);
            reserved = r.1;
        }
    }
    let slot = slots(&srv.0);
    if !slot.take(at_once(&c), SLOT_WAIT) {
        if let Some(w) = who {
            with_books(node, |b| {
                *b.credit.entry(w.to_string()).or_default() += reserved;
                b.dirty = true;
            });
        }
        return Err("the seller's model is busy: try again in a moment".into());
    }
    let started = Instant::now();
    let ran = run_server(&srv, &server_body(body, &model, cap), each);
    slot.give();
    let ms = started.elapsed().as_millis() as u64;
    let (tin, tout, first_ms, ok) = match &ran {
        Ok((i, o, f)) => (*i, *o, *f, true),
        Err(_) => (0, 0, 0, false),
    };
    // Settle: what it cost (an error costs nothing), never more than reserved plus what is left.
    let cost = if ok && who.is_some() { cost_mu(price, tin.max(1), tout) } else { 0 };
    let (charged, left) = match who {
        Some(w) => settle(node, w, reserved, cost, Use { at: now_s(), who: w.to_string(), model: model.clone(), tin, tout, mu: 0, ms, ok }),
        None => (0, 0),
    };
    ran?;
    Ok(json!({"arcade": {"usage": {"tokens_in": tin, "tokens_out": tout}, "cost": coins(charged / 1000), "cost_mu": charged, "left": coins(left / 1000),
                         "left_mu": left, "model": model, "first_ms": first_ms, "ms": ms, "seller": node.id_hex()}}))
}

/// Settle a request: give back what was reserved, take what it cost (never more than the buyer has), log it.
/// (charged, left), thousandths of a base unit.
fn settle(node: &Node, w: &str, reserved: u64, cost: u64, mut row: Use) -> (u64, u64) {
    with_books(node, |b| {
        let have = b.credit.entry(w.to_string()).or_default();
        *have += reserved;
        let charged = cost.min(*have);
        *have -= charged;
        let left = *have;
        if left == 0 && reserved == 0 && charged == 0 {
            b.credit.remove(w);
        }
        row.mu = charged;
        b.usage.push(row);
        if b.usage.len() > USAGE_KEEP {
            let drop = b.usage.len() - USAGE_KEEP;
            b.usage.drain(..drop);
        }
        b.dirty = true;
        (charged, left)
    })
}

// ---------------------------------------------------------------- serving speech (the seller's side)

/// What an audio piece handed on is: the first carries the audio's type; an empty piece with no type is a keepalive
/// while the server is still making the voice.
pub type AudioSink<'a> = dyn FnMut(Option<&str>, &[u8]) -> Result<(), String> + 'a;

/// Run one speech request on our own voice server: the audio to `each` as it comes (type first). (bytes, first_ms)
fn run_speech((url, key): &(String, String), body: &Value, each: &mut AudioSink) -> Result<(u64, u64), String> {
    enum Got {
        Head(String),
        Bytes(Vec<u8>),
        Fail(String),
    }
    let (tx, rx) = std::sync::mpsc::channel::<Got>();
    let (url, key, body) = (url.clone(), key.clone(), body.to_string());
    std::thread::Builder::new().name("llm speech".into()).spawn(move || {
        let mut r = agent(SPEECH_TIME.as_secs()).post(&format!("{url}/audio/speech")).set("Content-Type", "application/json");
        if !key.is_empty() {
            r = r.set("Authorization", &format!("Bearer {key}"));
        }
        let resp = match r.send_string(&body) {
            Ok(x) => x,
            Err(ureq::Error::Status(code, x)) => {
                let t = x.into_string().unwrap_or_default();
                let said = serde_json::from_str::<Value>(&t).ok().and_then(|v| v["error"]["message"].as_str().or(v["message"].as_str()).map(String::from)).unwrap_or(t);
                let _ = tx.send(Got::Fail(format!("the voice server said {code}: {}", said.chars().take(300).collect::<String>())));
                return;
            }
            Err(e) => {
                let _ = tx.send(Got::Fail(format!("the voice server did not answer: {e}")));
                return;
            }
        };
        let ct: String = resp.content_type().chars().filter(|c| c.is_ascii_alphanumeric() || "/.+-".contains(*c)).take(60).collect();
        if tx.send(Got::Head(if ct.is_empty() { "application/octet-stream".into() } else { ct })).is_err() {
            return;
        }
        let mut rd = resp.into_reader().take(MAX_AUDIO as u64 + 1);
        let mut buf = vec![0u8; PIECE];
        loop {
            match rd.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if tx.send(Got::Bytes(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Got::Fail(format!("the voice server's answer broke off: {e}")));
                    return;
                }
            }
        }
    }).map_err(err)?;
    let started = Instant::now();
    let (mut bytes, mut first_ms, mut ct) = (0u64, 0u64, None::<String>);
    loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Got::Head(t)) => ct = Some(t),
            Ok(Got::Bytes(b)) => {
                bytes += b.len() as u64;
                if bytes > MAX_AUDIO as u64 {
                    return Err("the voice server's answer is too large".into());
                }
                if first_ms == 0 {
                    first_ms = started.elapsed().as_millis() as u64;
                }
                each(ct.take().as_deref(), &b)?;
            }
            Ok(Got::Fail(e)) => return Err(e),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if started.elapsed() > SPEECH_TIME {
                    return Err("the voice server took too long".into());
                }
                each(None, &[])?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if bytes == 0 {
        return Err("the voice server sent no audio".into());
    }
    Ok((bytes, first_ms))
}

/// The body for the voice server: the passed fields only and the model.
fn speech_body(body: &Value, model: &str) -> Value {
    let mut out = Map::new();
    for k in SPEECH_PASSED {
        if let Some(v) = body.get(*k) {
            out.insert((*k).to_string(), v.clone());
        }
    }
    out.insert("model".into(), json!(model));
    Value::Object(out)
}

/// A speech request for `who` (an address, or None: this arcade's own operator): the price of its characters is
/// reserved, the audio goes to `each`, and it is charged only when audio came. The last event ({"arcade": ...}).
fn serve_speech(node: &Node, who: Option<&str>, body: &Value, each: &mut AudioSink) -> Result<Value, String> {
    let c = cfg(node);
    if !selling(&c) && who.is_some() {
        return Err("this arcade isn't sharing a model now".into());
    }
    let models: Vec<String> = c["models"].as_array().cloned().unwrap_or_default().iter().filter_map(|m| m.as_str().map(String::from)).collect();
    let model = body["model"].as_str().map(String::from).filter(|m| !m.is_empty())
        .or_else(|| models.iter().find(|m| kind_of(&c, m) == "speech").cloned()).ok_or("no speech model is shared here")?;
    if who.is_some() && !models.contains(&model) {
        return Err(format!("{model} isn't shared here (these are: {})", models.join(", ")));
    }
    if kind_of(&c, &model) != "speech" {
        return Err(format!("{model} isn't a speech model"));
    }
    let chars = body["input"].as_str().map(|t| t.trim().chars().count() as u64).unwrap_or(0);
    if chars == 0 || chars > MAX_SPEECH_CHARS {
        return Err(format!("input: 1 to {MAX_SPEECH_CHARS} characters"));
    }
    let srv = server_for(&c, &model)?;
    let free_own = who.is_none() && c["own_free"] != false;
    let price = if free_own { (0, 0) } else { (price_of(&c, &model).0, 0) };
    let cost = cost_mu(price, chars, 0);
    let mut reserved = 0u64;
    if let (Some(w), true) = (who, cost > 0) {
        reserved = with_books(node, |b| {
            let have = b.credit.get(w).copied().unwrap_or(0);
            if have < cost {
                return Err(format!("not enough credit: {} ARCADE left here, this costs {}", coins(have / 1000), coins(cost.div_ceil(1000))));
            }
            *b.credit.get_mut(w).unwrap() -= cost;
            b.dirty = true;
            Ok(cost)
        })?;
    }
    let slot = slots(&srv.0);
    if !slot.take(at_once(&c), SLOT_WAIT) {
        if let Some(w) = who {
            with_books(node, |b| {
                *b.credit.entry(w.to_string()).or_default() += reserved;
                b.dirty = true;
            });
        }
        return Err("the seller's voice is busy: try again in a moment".into());
    }
    let started = Instant::now();
    let ran = run_speech(&srv, &speech_body(body, &model), each);
    slot.give();
    let ms = started.elapsed().as_millis() as u64;
    let (bytes, first_ms, ok) = match &ran {
        Ok((b, f)) => (*b, *f, true),
        Err(_) => (0, 0, false),
    };
    let (charged, left) = match who {
        Some(w) => settle(node, w, reserved, if ok { cost } else { 0 }, Use { at: now_s(), who: w.to_string(), model: model.clone(), tin: chars, tout: 0, mu: 0, ms, ok }),
        None => (0, 0),
    };
    ran?;
    Ok(json!({"arcade": {"usage": {"characters": chars, "bytes": bytes}, "cost": coins(charged / 1000), "cost_mu": charged, "left": coins(left / 1000),
                         "left_mu": left, "model": model, "first_ms": first_ms, "ms": ms, "seller": node.id_hex()}}))
}

/// What a buyer has here, and where to pay.
fn balance_of(node: &Node, who: &str) -> Value {
    settle_claims(node);
    let (credit, waiting) = with_books(node, |b| (b.credit.get(who).copied().unwrap_or(0), b.claims.iter().filter(|x| x.0 == who).count()));
    json!({"credit": coins(credit / 1000), "credit_mu": credit, "waiting": waiting, "pay_to": crate::arcledger::own_address(node).ok(),
           "memo": String::from_utf8_lossy(&memo_of(who)), "refunds": cfg(node)["refunds"] != false})
}

/// A buyer says it paid: kept until the payment is in a block.
fn note_paid(node: &Node, who: &str, txid: &str) -> Result<Value, String> {
    if !crate::udpsvc::is_hex(txid, 64) {
        return Err("not a txid".into());
    }
    with_books(node, |b| {
        if b.claimed.contains(txid) || b.claims.iter().any(|x| x.1 == txid) {
            return;
        }
        if b.claims.len() < 10_000 {
            b.claims.push((who.to_string(), txid.to_string(), now_s()));
            b.dirty = true;
        }
    });
    Ok(balance_of(node, who))
}

/// Pay a buyer's unused credit back.
fn refund(node: &Node, who: &str) -> Result<Value, String> {
    if cfg(node)["refunds"] == false {
        return Err("this seller doesn't give credit back".into());
    }
    let units = with_books(node, |b| {
        let mu = b.credit.remove(who).unwrap_or(0);
        b.dirty = true;
        mu / 1000
    });
    let fee_room = arcade_arc::params::DUST * 2;
    if units <= fee_room {
        if units > 0 {
            with_books(node, |b| {
                *b.credit.entry(who.to_string()).or_default() += units * 1000;
                b.dirty = true;
            });
        }
        return Err("too little credit to send back".into());
    }
    match crate::arcledger::pay_with_memo(node, who, units, b"arcade-llm refund") {
        Ok((txid, _)) => {
            with_books(node, |b| {
                b.claimed.insert(txid.clone());
                b.dirty = true;
            });
            let _ = node.app.event("llm", &format!("gave {} ARCADE of model credit back to {} ({txid})", coins(units), short(who)));
            Ok(json!({"refunded": coins(units), "txid": txid}))
        }
        Err(e) => {
            with_books(node, |b| {
                *b.credit.entry(who.to_string()).or_default() += units * 1000;
                b.dirty = true;
            });
            Err(format!("the seller couldn't pay it back now: {e}"))
        }
    }
}

// ---------------------------------------------------------------- frames in (both sides)

#[derive(Default)]
struct Asm {
    buf: Vec<u8>,
    seq: u64,
    at: Option<Instant>,
}

fn asm() -> &'static Mutex<HashMap<(String, u64), Asm>> {
    static A: LazyLock<Mutex<HashMap<(String, u64), Asm>>> = LazyLock::new(Default::default);
    &A
}

/// An `lm` frame over a link from `peer` that isn't an answer to us (crvia takes those).
pub fn on_frame(node: &Node, peer: &str, f: &Frame) {
    match f.head["op"].as_str().unwrap_or("") {
        "offer" => on_offer(node, &f.head),
        "q" => on_piece(node, peer, f),
        _ => {}
    }
}

fn answer(peer: &str, id: u64, seq: u64, end: bool, ev: Value) {
    let _ = crate::crvia::send(peer, Frame::new(json!({"t": "lm", "op": "a", "id": id, "seq": seq, "end": end, "ev": ev})));
}

/// Queue a frame for `peer`, waiting while its link's queue is full (audio comes faster than a link sends).
fn send_wait(peer: &str, f: Frame) -> Result<(), String> {
    let end = Instant::now() + GAP_TIME;
    loop {
        match crate::crvia::send(peer, f.clone()) {
            Ok(()) => return Ok(()),
            Err(e) if e.contains("not sending") && Instant::now() < end => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("the buyer's link: {e}")),
        }
    }
}

fn answer_err(peer: &str, id: u64, seq: u64, e: &str) {
    let _ = crate::crvia::send(peer, Frame::new(json!({"t": "lm", "op": "a", "id": id, "seq": seq, "end": true, "err": e})));
}

fn on_piece(node: &Node, peer: &str, f: &Frame) {
    let Some(id) = f.head["id"].as_u64() else { return };
    let seq = f.head["seq"].as_u64().unwrap_or(0);
    let end = f.head["end"] == true;
    let whole = {
        let mut a = asm().lock().unwrap();
        a.retain(|_, x| x.at.is_some_and(|t| t.elapsed() < Duration::from_secs(300)));
        if a.len() > 256 && !a.contains_key(&(peer.to_string(), id)) {
            drop(a);
            return answer_err(peer, id, 0, "the seller is taking too many requests");
        }
        let x = a.entry((peer.to_string(), id)).or_default();
        x.at.get_or_insert_with(Instant::now);
        if seq != x.seq || x.buf.len() + f.blob.len() > MAX_REQUEST {
            a.remove(&(peer.to_string(), id));
            drop(a);
            return answer_err(peer, id, 0, "the request came in broken or too large");
        }
        x.seq += 1;
        x.buf.extend_from_slice(&f.blob);
        if !end {
            return;
        }
        a.remove(&(peer.to_string(), id)).map(|x| x.buf).unwrap_or_default()
    };
    let mut raw = Vec::new();
    if flate2::read::ZlibDecoder::new(&whole[..]).take(MAX_REQUEST as u64 + 1).read_to_end(&mut raw).is_err() || raw.len() > MAX_REQUEST {
        return answer_err(peer, id, 0, "the request does not inflate");
    }
    let Ok(req) = serde_json::from_slice::<Value>(&raw) else { return answer_err(peer, id, 0, "the request isn't JSON") };
    let n = node.self_arc();
    let peer = peer.to_string();
    let _ = std::thread::Builder::new().name("llm serve".into()).spawn(move || serve_request(&n, &peer, id, &req));
}

fn serve_request(node: &Node, peer: &str, id: u64, req: &Value) {
    let who = match grant_ok(&req["grant"], &node.id_hex()) {
        Ok(w) => w,
        Err(e) => return answer_err(peer, id, 0, &e),
    };
    let out = match req["what"].as_str().unwrap_or("") {
        "balance" => Ok(balance_of(node, &who)),
        "paid" => note_paid(node, &who, req["txid"].as_str().unwrap_or("")),
        "refund" => refund(node, &who),
        "chat" => {
            let mut seq = 0u64;
            let r = serve_chat(node, Some(&who), &req["body"], &mut |chunk| {
                if !crate::crvia::linked(peer) {
                    return Err("the buyer's link went down".into());
                }
                answer(peer, id, seq, false, chunk);
                seq += 1;
                Ok(())
            });
            match r {
                Ok(last) => return answer(peer, id, seq, true, last),
                Err(e) => return answer_err(peer, id, seq, &e),
            }
        }
        "speech" => {
            let mut seq = 0u64;
            let r = serve_speech(node, Some(&who), &req["body"], &mut |ct, bytes| {
                if !crate::crvia::linked(peer) {
                    return Err("the buyer's link went down".into());
                }
                let ev = match ct {
                    Some(t) => json!({"type": t}),
                    None if bytes.is_empty() => json!({"wait": true}),
                    None => Value::Null,
                };
                let mut f = Frame::new(json!({"t": "lm", "op": "a", "id": id, "seq": seq, "end": false, "ev": ev}));
                f.blob = bytes.to_vec();
                send_wait(peer, f)?;
                seq += 1;
                Ok(())
            });
            match r {
                Ok(last) => return answer(peer, id, seq, true, last),
                Err(e) => return answer_err(peer, id, seq, &e),
            }
        }
        w => Err(format!("unknown ask {w}")),
    };
    match out {
        Ok(v) => answer(peer, id, 0, true, v),
        Err(e) => answer_err(peer, id, 0, &e),
    }
}

// ---------------------------------------------------------------- asking a seller (the buyer's side)

/// Make sure there is a mesh link to `seller` (dial its public name from its offer if there is none), up to 15 s.
fn reach(node: &Node, seller: &str) -> Result<(), String> {
    if crate::crvia::linked(seller) {
        return Ok(());
    }
    let o = offer_of(node, seller).ok_or("that seller has no offer now")?;
    let name = o["name"].as_str().filter(|n| !n.is_empty()).ok_or("no mesh link to that seller, and it has no public name to dial (it is reached once it links to this arcade or a relay)")?;
    crate::meshsvc::dial(&node.self_arc(), name.to_string());
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        if crate::crvia::linked(seller) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!("couldn't link to that seller at {name}"))
}

/// Ask `seller` `what` (the grant is added); each answer event but the last to `each`; the last returned.
fn ask(node: &Node, seller: &str, req: Value, each: &mut dyn FnMut(Value) -> Result<(), String>) -> Result<Value, String> {
    ask_frames(node, seller, req, REQUEST_TIME, &mut |f| each(f.head["ev"].clone()))
}

/// As `ask`, with each answer frame whole (its blob too), and the longest the whole may take.
fn ask_frames(node: &Node, seller: &str, mut req: Value, total: Duration, each: &mut dyn FnMut(&Frame) -> Result<(), String>) -> Result<Value, String> {
    reach(node, seller)?;
    req["grant"] = my_grant(node, seller)?;
    let raw = req.to_string();
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    z.write_all(raw.as_bytes()).map_err(err)?;
    let packed = z.finish().map_err(err)?;
    let mut via = crate::crvia::Via::open(seller)?;
    let pieces: Vec<&[u8]> = if packed.is_empty() { vec![&packed[..]] } else { packed.chunks(PIECE).collect() };
    let n = pieces.len();
    for (i, p) in pieces.into_iter().enumerate() {
        let mut f = Frame::new(json!({"t": "lm", "op": "q", "id": via.id, "seq": i, "end": i + 1 == n}));
        f.blob = p.to_vec();
        crate::crsource::Pipe::send(&mut via, &f)?;
    }
    let end = Instant::now() + total;
    let mut want = 0u64;
    loop {
        let until = (Instant::now() + GAP_TIME).min(end);
        let f = crate::crsource::Pipe::recv(&mut via, until)?;
        if f.head["seq"].as_u64() != Some(want) {
            return Err("the seller's answers came out of order".into());
        }
        want += 1;
        if let Some(e) = f.head["err"].as_str() {
            return Err(e.to_string());
        }
        if f.head["end"] == true {
            return Ok(f.head["ev"].clone());
        }
        each(&f)?;
    }
}

/// A rough count of tokens in text (for checking a seller's bill).
fn count_rough(s: &str) -> u64 {
    (s.len() as u64).div_ceil(4)
}

/// Chat with `seller` (ours: run here, free for the operator), streaming chunks to `each`. The last event's "arcade".
pub fn chat(node: &Node, seller: &str, body: &Value, each: &mut dyn FnMut(Value) -> Result<(), String>) -> Result<Value, String> {
    let started = Instant::now();
    let mut first_ms = 0u64;
    let mut text = 0u64;
    let mut count = |chunk: &Value| {
        if first_ms == 0 {
            first_ms = started.elapsed().as_millis() as u64;
        }
        for ch in chunk["choices"].as_array().into_iter().flatten() {
            let d = &ch["delta"];
            text += count_rough(d["content"].as_str().unwrap_or("")) + count_rough(d["reasoning_content"].as_str().or(d["reasoning"].as_str()).unwrap_or(""));
            if d["tool_calls"].is_array() {
                text += count_rough(&d["tool_calls"].to_string());
            }
        }
    };
    if seller == node.id_hex() {
        let last = serve_chat(node, None, body, &mut |c| {
            count(&c);
            each(c)
        })?;
        return Ok(last["arcade"].clone());
    }
    let r = ask(node, seller, json!({"what": "chat", "body": body}), &mut |c| {
        count(&c);
        each(c)
    });
    let ms = started.elapsed().as_millis() as u64;
    with_records(node, |rec| {
        let s = rec.sellers.entry(seller.to_string()).or_default();
        s.requests += 1;
        match &r {
            Ok(last) => {
                let a = &last["arcade"];
                s.billed_out += a["usage"]["tokens_out"].as_u64().unwrap_or(0);
                s.counted_out += text;
                s.first_ms += first_ms;
                s.out_ms += ms;
                s.spent_mu += a["cost_mu"].as_u64().unwrap_or(0);
                if let Some(l) = a["left_mu"].as_u64() {
                    rec.credit.insert(seller.to_string(), l);
                }
            }
            // The buyer's own lack of credit is no failure of the seller's.
            Err(e) if e.starts_with("not enough credit") => s.requests -= 1,
            Err(_) => s.failed += 1,
        }
    });
    let last = r?;
    auto_topup(node, seller, last["arcade"]["left_mu"].as_u64().unwrap_or(u64::MAX), last["arcade"]["cost_mu"].as_u64().unwrap_or(0));
    Ok(last["arcade"].clone())
}

/// Speech from `seller` (ours: run here, free for the operator): the audio to `each` as it comes, its type first.
/// The last event's "arcade".
pub fn speak(node: &Node, seller: &str, body: &Value, each: &mut AudioSink) -> Result<Value, String> {
    let started = Instant::now();
    let mut first_ms = 0u64;
    let mut sink = |ct: Option<&str>, b: &[u8]| -> Result<(), String> {
        if b.is_empty() && ct.is_none() {
            return Ok(()); // a keepalive
        }
        if first_ms == 0 {
            first_ms = started.elapsed().as_millis() as u64;
        }
        each(ct, b)
    };
    if seller == node.id_hex() {
        let last = serve_speech(node, None, body, &mut sink)?;
        return Ok(last["arcade"].clone());
    }
    let chars = body["input"].as_str().map(|t| t.trim().chars().count() as u64).unwrap_or(0);
    if chars == 0 || chars > MAX_SPEECH_CHARS {
        return Err(format!("input: 1 to {MAX_SPEECH_CHARS} characters"));
    }
    let r = ask_frames(node, seller, json!({"what": "speech", "body": body}), SPEECH_TIME, &mut |f| {
        let ev = &f.head["ev"];
        if ev["wait"] == true {
            return Ok(());
        }
        sink(ev["type"].as_str(), &f.blob)
    });
    let ms = started.elapsed().as_millis() as u64;
    with_records(node, |rec| {
        let s = rec.sellers.entry(seller.to_string()).or_default();
        s.requests += 1;
        match &r {
            Ok(last) => {
                let a = &last["arcade"];
                // For speech the bill is characters, which we count exactly.
                s.billed_out += a["usage"]["characters"].as_u64().unwrap_or(0);
                s.counted_out += chars;
                s.first_ms += first_ms;
                s.out_ms += ms;
                s.spent_mu += a["cost_mu"].as_u64().unwrap_or(0);
                if let Some(l) = a["left_mu"].as_u64() {
                    rec.credit.insert(seller.to_string(), l);
                }
            }
            Err(e) if e.starts_with("not enough credit") => s.requests -= 1,
            Err(_) => s.failed += 1,
        }
    });
    let last = r?;
    auto_topup(node, seller, last["arcade"]["left_mu"].as_u64().unwrap_or(u64::MAX), last["arcade"]["cost_mu"].as_u64().unwrap_or(0));
    Ok(last["arcade"].clone())
}

/// `llm.balance {seller}`: what this wallet has at a seller, and where to pay it.
pub fn balance(node: &Node, req: &Value) -> Result<Value, String> {
    let seller = seller_arg(node, req)?;
    let v = ask(node, &seller, json!({"what": "balance"}), &mut |_| Ok(()))?;
    if let Some(c) = v["credit_mu"].as_u64() {
        with_records(node, |r| {
            r.credit.insert(seller.clone(), c);
        });
    }
    Ok(v)
}

fn seller_arg(node: &Node, req: &Value) -> Result<String, String> {
    let s = req["seller"].as_str().map(|s| s.trim().trim_start_matches('@').to_lowercase()).filter(|s| !s.is_empty()).ok_or("seller: a node id or @tag")?;
    if crate::udpsvc::is_hex(&s, 64) {
        return Ok(s);
    }
    offers(node).into_iter().find(|o| o["tag"].as_str() == Some(s.as_str()) || o["o"].as_str().is_some_and(|id| id.starts_with(&s) && s.len() >= 8))
        .and_then(|o| o["o"].as_str().map(String::from)).ok_or_else(|| format!("no seller {s} on offer now"))
}

/// Buy `units` of credit at `seller`: pay its address with our memo, then tell it the txid.
fn buy(node: &Node, seller: &str, units: u64) -> Result<Value, String> {
    if units < arcade_arc::params::DUST * 10 {
        return Err("buy at least 0.01 ARCADE".into());
    }
    let b = ask(node, seller, json!({"what": "balance"}), &mut |_| Ok(()))?;
    let pay_to = b["pay_to"].as_str().filter(|a| a.len() <= 64).ok_or("that seller has no ARCADE address")?;
    let me = crate::arcledger::own_address(node)?;
    let (txid, fee) = crate::arcledger::pay_with_memo(node, pay_to, units, &memo_of(&me))?;
    with_records(node, |r| r.sellers.entry(seller.to_string()).or_default().paid += units);
    let said = ask(node, seller, json!({"what": "paid", "txid": txid}), &mut |_| Ok(()));
    let _ = node.app.event("llm", &format!("bought {} ARCADE of model credit from {} (fee {}): {txid}", coins(units), short(seller), coins(fee)));
    Ok(json!({"txid": txid, "fee": coins(fee), "paid": coins(units), "pay_to": pay_to, "told": said.is_ok(),
              "note": "credited once the payment is in a block (about 2.5 minutes)"}))
}

/// `llm.topup {seller, amount}` (ARCADE).
pub fn topup(node: &Node, req: &Value) -> Result<Value, String> {
    let seller = seller_arg(node, req)?;
    let amount = req["amount"].as_f64().or_else(|| req["amount"].as_str().and_then(|s| s.parse().ok())).ok_or("amount: ARCADE to buy")?;
    if !(0.0..=100_000.0).contains(&amount) {
        return Err("amount: 0.01 to 100000 ARCADE".into());
    }
    buy(node, &seller, to_units(amount))
}

/// `llm.refund {seller}`: ask a seller for our unused credit back.
pub fn ask_refund(node: &Node, req: &Value) -> Result<Value, String> {
    let seller = seller_arg(node, req)?;
    let v = ask(node, &seller, json!({"what": "refund"}), &mut |_| Ok(()))?;
    with_records(node, |r| {
        r.credit.insert(seller.clone(), 0);
    });
    Ok(v)
}

// ---------------------------------------------------------------- auto top-up (the buyer's choice)

fn auto_cfg(node: &Node) -> Value {
    read_meta(node, b"llm_auto").unwrap_or(json!({"on": false, "floor": 0.5, "amount": 2.0, "daily_cap": 10.0}))
}

/// `llm.auto {on?, floor?, amount?, daily_cap?}` (ARCADE): buy again when a seller's credit falls under the floor.
pub fn auto_set(node: &Node, req: &Value) -> Result<Value, String> {
    let mut a = auto_cfg(node);
    if let Some(b) = req["on"].as_bool() {
        a["on"] = json!(b);
    }
    for k in ["floor", "amount", "daily_cap"] {
        if let Some(x) = req[k].as_f64() {
            if !(0.0..=100_000.0).contains(&x) {
                return Err(format!("{k}: 0 to 100000 ARCADE"));
            }
            a[k] = json!(x);
        }
    }
    write_meta(node, b"llm_auto", &a)?;
    let (day, spent) = with_records(node, |r| r.auto_day.clone());
    a["spent_today"] = json!(if day == today() { coins(spent) } else { 0.0 });
    Ok(a)
}

fn today() -> String {
    format!("{}", now_s() / 86400)
}

fn auto_topup(node: &Node, seller: &str, left_mu: u64, cost_mu: u64) {
    let a = auto_cfg(node);
    if a["on"] != true || cost_mu == 0 {
        return;
    }
    let floor = to_units(a["floor"].as_f64().unwrap_or(0.0)).saturating_mul(1000);
    let amount = to_units(a["amount"].as_f64().unwrap_or(0.0));
    let cap = to_units(a["daily_cap"].as_f64().unwrap_or(0.0));
    if left_mu >= floor || amount == 0 {
        return;
    }
    let ok = with_records(node, |r| {
        if r.auto_day.0 != today() {
            r.auto_day = (today(), 0);
        }
        if r.auto_day.1 + amount > cap {
            return false;
        }
        r.auto_day.1 += amount;
        true
    });
    if !ok {
        let _ = node.app.event("llm", "model credit is low and today's auto top-up limit is reached");
        return;
    }
    let n = node.self_arc();
    let seller = seller.to_string();
    let _ = std::thread::Builder::new().name("llm topup".into()).spawn(move || {
        if let Err(e) = buy(&n, &seller, amount) {
            with_records(&n, |r| r.auto_day.1 = r.auto_day.1.saturating_sub(amount));
            let _ = n.app.event("llm", &format!("auto top-up at {} failed: {e}", short(&seller)));
        }
    });
}

// ---------------------------------------------------------------- the OpenAI door's keys

fn key_hash(secret: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(secret.as_bytes()))
}

/// `llm.key_new {name?}`: a key for OpenAI programs (sk-arcade-...), shown once.
pub fn key_new(node: &Node, req: &Value) -> Result<Value, String> {
    let mut r = [0u8; 24];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut r);
    let id = hex::encode(&r[..4]);
    let key = format!("sk-arcade-{id}_{}", hex::encode(&r[4..]));
    let name: String = req["name"].as_str().unwrap_or("").chars().take(60).collect();
    write_meta(node, format!("llm_key:{id}").as_bytes(), &json!({"id": id, "name": name, "hash": key_hash(&key), "created": now_s()}))?;
    Ok(json!({"key": key, "id": id, "name": name, "base_url": format!("http://127.0.0.1:{}/openai/v1", node.http_port.get().copied().unwrap_or(8420))}))
}

/// `llm.keys`: the keys made (never the secrets).
pub fn keys(node: &Node) -> Result<Value, String> {
    let rows: Vec<Value> = node.app.store.read(|r| r.scan_prefix(arcade_app::META, b"llm_key:")).map_err(err)?.into_iter()
        .filter_map(|(_, v)| serde_json::from_slice::<Value>(&v).ok()).map(|mut v| {
            v.as_object_mut().map(|o| o.remove("hash"));
            v
        }).collect();
    Ok(json!({"keys": rows, "base_url": format!("http://127.0.0.1:{}/openai/v1", node.http_port.get().copied().unwrap_or(8420))}))
}

/// `llm.key_drop {id}`.
pub fn key_drop(node: &Node, req: &Value) -> Result<Value, String> {
    let id = req["id"].as_str().filter(|i| crate::udpsvc::is_hex(i, 8)).ok_or("id: the key's id")?;
    node.app.store.write(|w| w.del(arcade_app::META, format!("llm_key:{id}").as_bytes())).map_err(err)?;
    Ok(json!({"dropped": id}))
}

/// Is `bearer` one of our keys?
pub fn key_ok(node: &Node, bearer: &str) -> bool {
    let Some(id) = bearer.strip_prefix("sk-arcade-").and_then(|k| k.split_once('_')).map(|x| x.0) else { return false };
    read_meta(node, format!("llm_key:{id}").as_bytes()).is_some_and(|v| v["hash"] == key_hash(bearer).as_str())
}

/// A model name as OpenAI programs use it: "model@seller" (tag, else the node id's first 12), the seller's id.
fn model_names(node: &Node) -> Vec<(String, String, String)> {
    model_list(node).into_iter().map(|x| (x.0, x.1, x.2)).collect()
}

/// As `model_names`, with each model's offer entry.
fn model_list(node: &Node) -> Vec<(String, String, String, Value)> {
    let mut v = Vec::new();
    for o in offers(node) {
        let id = o["o"].as_str().unwrap_or("").to_string();
        let who = o["tag"].as_str().map(String::from).unwrap_or_else(|| id[..12.min(id.len())].to_string());
        for m in o["models"].as_array().into_iter().flatten() {
            if let Some(mid) = m["id"].as_str() {
                v.push((format!("{mid}@{who}"), mid.to_string(), id.clone(), m.clone()));
            }
        }
    }
    v
}

/// `GET /openai/v1/models`.
pub fn openai_models(node: &Node) -> Value {
    let data: Vec<Value> = model_list(node).into_iter().map(|(name, _, seller, m)| {
        let mut x = json!({"id": name, "object": "model", "created": 0, "owned_by": seller});
        if m["kind"] == "speech" {
            x["kind"] = json!("speech");
            x["voices"] = m["voices"].clone();
        }
        x
    }).collect();
    json!({"object": "list", "data": data})
}

/// Which seller and model an OpenAI "model" names: "model@seller", or a bare model: ours, else a seller we have credit
/// at, else a free one, else the cheapest.
pub fn pick(node: &Node, asked: &str) -> Result<(String, String), String> {
    let names = model_names(node);
    if let Some(x) = names.iter().find(|x| x.0 == asked) {
        return Ok((x.2.clone(), x.1.clone()));
    }
    let me = node.id_hex();
    let mut c: Vec<&(String, String, String)> = names.iter().filter(|x| x.1 == asked).collect();
    if c.is_empty() {
        return Err(format!("no seller offers {asked} now (GET /openai/v1/models lists what is on offer)"));
    }
    let credit = records_loaded(node).credit.clone();
    let os = offers(node);
    let price = |seller: &str| -> u64 {
        os.iter().find(|o| o["o"] == seller).and_then(|o| o["models"].as_array().and_then(|m| m.iter().find(|m| m["id"] == asked)).map(|m| m["in"].as_u64().unwrap_or(0) + m["out"].as_u64().unwrap_or(0))).unwrap_or(u64::MAX)
    };
    c.sort_by_key(|x| (x.2 != me, credit.get(&x.2).copied().unwrap_or(0) == 0, price(&x.2)));
    Ok((c[0].2.clone(), c[0].1.clone()))
}

// ---------------------------------------------------------------- the service

/// `llm.chat {seller, model?, messages, ...}`: one reply, whole (the Models tab's try-it box).
pub fn chat_cmd(node: &Node, req: &Value) -> Result<Value, String> {
    let (seller, model) = match req["seller"].as_str() {
        Some(_) => (seller_arg(node, req)?, req["model"].as_str().unwrap_or("").to_string()),
        None => pick(node, req["model"].as_str().ok_or("model or seller")?)?,
    };
    let mut body = req.clone();
    body["model"] = json!(model);
    let (mut content, mut reasoning) = (String::new(), String::new());
    let last = chat(node, &seller, &body, &mut |c| {
        for ch in c["choices"].as_array().into_iter().flatten() {
            content.push_str(ch["delta"]["content"].as_str().unwrap_or(""));
            reasoning.push_str(ch["delta"]["reasoning_content"].as_str().or(ch["delta"]["reasoning"].as_str()).unwrap_or(""));
        }
        Ok(())
    })?;
    Ok(json!({"content": content, "reasoning": reasoning, "seller": seller, "model": model, "usage": last["usage"], "cost": last["cost"], "left": last["left"],
              "first_ms": last["first_ms"], "ms": last["ms"]}))
}

/// `llm.speak {seller?, model, input, voice?, response_format?}`: speech, whole, as base64 (the Models tab's try-it box).
pub fn speak_cmd(node: &Node, req: &Value) -> Result<Value, String> {
    use base64::Engine;
    let (seller, model) = match req["seller"].as_str() {
        Some(_) => (seller_arg(node, req)?, req["model"].as_str().unwrap_or("").to_string()),
        None => pick(node, req["model"].as_str().ok_or("model or seller")?)?,
    };
    let mut body = json!({"model": model, "input": req["input"], "response_format": req["response_format"].as_str().unwrap_or("wav")});
    if let Some(v) = req["voice"].as_str() {
        body["voice"] = json!(v);
    }
    let (mut ty, mut audio) = (String::new(), Vec::new());
    let last = speak(node, &seller, &body, &mut |ct, b| {
        if let Some(t) = ct {
            ty = t.to_string();
        }
        if audio.len() + b.len() > 24 << 20 {
            return Err("too long to try here: use the OpenAI door".into());
        }
        audio.extend_from_slice(b);
        Ok(())
    })?;
    Ok(json!({"type": ty, "audio": base64::engine::general_purpose::STANDARD.encode(&audio), "seller": seller, "model": model, "usage": last["usage"],
              "cost": last["cost"], "left": last["left"], "first_ms": last["first_ms"], "ms": last["ms"]}))
}

/// `llm.usage`: what this arcade sold lately (seller), and its record of the sellers it bought from.
pub fn usage(node: &Node, req: &Value) -> Value {
    let n = req["n"].as_u64().unwrap_or(50).min(500) as usize;
    let rows: Vec<Value> = with_books(node, |b| b.usage.iter().rev().take(n).map(|u| json!({"at": u.at, "buyer": u.who, "model": u.model,
        "tokens_in": u.tin, "tokens_out": u.tout, "arcade": coins(u.mu / 1000), "ms": u.ms, "ok": u.ok})).collect());
    let sellers = with_records(node, |r| serde_json::to_value(&r.sellers).unwrap_or(Value::Null));
    json!({"sold": rows, "bought_from": sellers})
}

/// Started once: say our offer every OFFER_EVERY and settle payments said to us.
pub fn run(node: std::sync::Arc<Node>) {
    std::thread::sleep(Duration::from_secs(20));
    loop {
        let due = seen().lock().unwrap().last_said.is_none_or(|t| t.elapsed() >= OFFER_EVERY);
        if due {
            say_offer(&node);
        }
        settle_claims(&node);
        std::thread::sleep(Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_and_what_credit_covers() {
        // 1 ARCADE per million in, 2 per million out.
        let p = (COIN, 2 * COIN);
        // 1000 in + 500 out = 0.001 + 0.001 ARCADE = 200_000 base units = 200_000_000 mu.
        assert_eq!(cost_mu(p, 1000, 500), 200_000_000);
        assert_eq!(cost_mu((0, 0), 1_000_000, 1_000_000), 0);
        // With 0.002 ARCADE: after 1000 in (0.001), 0.001 left = 500 tokens out.
        assert_eq!(affordable_out(p, 200_000_000, 1000), Some(500));
        assert_eq!(affordable_out(p, 50_000_000, 1000), Some(0));
        assert_eq!(affordable_out((COIN, 0), 200_000_000, 1000), None);
        assert_eq!(affordable_out((COIN, 0), 10, 1000), Some(0));
    }

    #[test]
    fn server_body_passes_only_known_fields_and_caps() {
        let b = json!({"messages": [{"role": "user", "content": "hi"}], "max_tokens": 9000, "temperature": 0.2, "url": "http://evil", "stream": false});
        let s = server_body(&b, "m", 1000);
        assert_eq!(s["max_tokens"], 1000);
        assert_eq!(s["model"], "m");
        assert_eq!(s["stream"], true);
        assert!(s.get("url").is_none());
        assert_eq!(s["temperature"], 0.2);
        let s = server_body(&json!({"messages": [], "max_completion_tokens": 10}), "m", 1000);
        assert_eq!(s["max_tokens"], 10);
        assert!(s.get("max_completion_tokens").is_none());
    }

    #[test]
    fn grants_check_seller_address_and_expiry() {
        let sk = arcade_wallet::SecretKey::from_slice(&[7u8; 32]).unwrap();
        let pk = arcade_wallet::secp256k1::PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
        let address = arcade_wallet::keys::p2pkh_address(&pk, 0x71);
        let seller = "ab".repeat(32);
        let exp = now_s() + 1000;
        let sig = arcade_wallet::keys::signmessage(&sk, MAGIC, &grant_words(&seller, &address, exp));
        let g = json!({"address": address, "expires": exp, "sig": sig});
        assert_eq!(grant_ok(&g, &seller).unwrap(), address);
        assert!(grant_ok(&g, &"cd".repeat(32)).is_err(), "made for another seller");
        let mut late = g.clone();
        late["expires"] = json!(exp + 1);
        assert!(grant_ok(&late, &seller).is_err(), "changed expiry");
        let old = json!({"address": address, "expires": 5, "sig": arcade_wallet::keys::signmessage(&sk, MAGIC, &grant_words(&seller, &address, 5))});
        assert!(grant_ok(&old, &seller).is_err(), "ran out");
    }

    #[test]
    fn servers_old_and_new_and_where_models_live() {
        let old = json!({"url": "http://127.0.0.1:18300/v1/", "key": "k"});
        assert_eq!(servers(&old), vec![("http://127.0.0.1:18300/v1".to_string(), "k".to_string())]);
        let c = json!({"servers": [{"url": "http://127.0.0.1:18300"}, {"url": "http://127.0.0.1:8096/v1", "key": "v"}, {"url": "ftp://x"}],
                       "info": {"csm-1b": {"server": "http://127.0.0.1:8096/v1", "kind": "speech"}}});
        assert_eq!(servers(&c).len(), 2);
        assert_eq!(server_for(&c, "csm-1b").unwrap(), ("http://127.0.0.1:8096/v1".to_string(), "v".to_string()));
        assert_eq!(server_for(&c, "qwen").unwrap().0, "http://127.0.0.1:18300/v1");
        assert_eq!(kind_of(&c, "csm-1b"), "speech");
        assert_eq!(kind_of(&c, "qwen"), "chat");
        // A key left out keeps the one kept for that address.
        let s = servers_asked(&c, &json!({"servers": [{"url": "http://127.0.0.1:8096/v1"}, {"url": "http://127.0.0.1:9000", "key": "n"}]})).unwrap().unwrap();
        assert_eq!(s[0]["key"], "v");
        assert_eq!(s[1], json!({"url": "http://127.0.0.1:9000/v1", "key": "n"}));
        assert!(servers_asked(&c, &json!({"url": "file:///etc"})).is_err());
    }

    #[test]
    fn speech_models_are_known_and_only_known_fields_pass() {
        assert_eq!(kind_in_list(&json!({"id": "csm-1b", "voices": ["narrator"]})), "speech");
        assert_eq!(kind_in_list(&json!({"id": "my-voice", "voices": []})), "speech");
        assert_eq!(kind_in_list(&json!({"id": "kokoro-82m"})), "speech");
        assert_eq!(kind_in_list(&json!({"id": "qwen3.8-flash-next"})), "chat");
        let b = speech_body(&json!({"model": "x", "input": "hi", "voice": "narrator", "response_format": "wav", "url": "http://evil"}), "csm-1b");
        assert_eq!(b, json!({"model": "csm-1b", "input": "hi", "voice": "narrator", "response_format": "wav"}));
        // 1 ARCADE per million characters: 1000 characters cost 0.001 ARCADE.
        assert_eq!(cost_mu((COIN, 0), 1000, 0), 100_000_000);
    }

    #[test]
    fn memo_names_the_buyer() {
        assert_eq!(memo_of("aSBD9BhX"), b"arcade-llm aSBD9BhX".to_vec());
    }
}
