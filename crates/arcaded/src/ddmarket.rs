//! DigiDollar for DGB on one DigiByte chain (docs/MARKET.md "DigiDollar", build step 4), V1.0 with V1.0.
//!
//! The maker's own daemon holds its orders and answers for them; nobody else holds a key or a signature.
//! A taker asks the maker's arcade over a mesh link (`sh` question `mk`):
//!   book                       the maker's open orders on a chain
//!   fill {order, cents, taker} the maker lays out one transaction with both sides' coins (arcade_wallet
//!                              ddtrade) and sends it back unsigned, its own coins held for FILL_LIFE
//!   sign {fill, part}          the taker's signatures over it; the maker checks the whole transaction as
//!                              its own side, signs last, and broadcasts
//! An order is {side: sell|buy (the maker's dollars), cents, dgb}: a fill of part pays its share of `dgb`,
//! rounded against the taker; a fill and what is left are each $1 or more, in whole dollars (DigiDollars are
//! not divided here; amounts stay in cents, as the chain's record counts them). The taker pays the network fee.

use crate::node::Node;
use arcade_chain::params::{by_key, ChainParams};
use arcade_chain::{Hash256, OutPoint, Tx};
use arcade_wallet::dd::Held;
use arcade_wallet::ddtrade::{self, Deal, Part, Role, Side, Unsigned};
use arcade_wallet::tx::{TxOutSpec, Utxo};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// How long a maker holds its coins for a fill it laid out and the taker has not signed. An arcade asking again for
/// the same order replaces its fill without holding it much longer (SIGN_SECS at most past this), and one whose fill
/// ran out unsigned waits as long again before it may hold that order again.
const FILL_LIFE: u64 = 600;
/// What a fill laid out again for the same arcade keeps at least, for its signatures (they follow the ask at once).
const SIGN_SECS: u64 = 120;
/// Fills one asking arcade may have laid out here at once (one per order).
const FILLS_PER_PEER: usize = 3;
const LEAST: i64 = arcade_wallet::dd::LEAST_OUTPUT;

fn key(id: &str) -> Vec<u8> {
    format!("ddorder:{id}").into_bytes()
}

fn get(node: &Node, id: &str) -> Option<Value> {
    node.app.store.read(|r| r.get(arcade_app::META, &key(id))).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok())
}

fn put(node: &Node, o: &Value) {
    let _ = node.app.store.write(|w| w.put(arcade_app::META, &key(o["id"].as_str().unwrap_or("")), o.to_string().as_bytes()));
}

/// This arcade's own DigiDollar orders (open ones unless `all`).
pub fn orders(node: &Node, chain: Option<&str>, all: bool) -> Vec<Value> {
    let now = arcade_app::now();
    node.app
        .store
        .read(|r| r.scan_prefix(arcade_app::META, b"ddorder:"))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_slice::<Value>(&v).ok())
        .filter(|o| chain.is_none_or(|c| o["chain"] == c))
        .filter(|o| all || (o["status"] == "open" && o["expires"].as_u64().is_none_or(|e| e > now)))
        .collect()
}

pub fn dd_chain(key: &str) -> Option<&'static ChainParams> {
    by_key(key).filter(|p| matches!(p.key, "dgb" | "dgbtest"))
}

/// The DGB a fill of `cents` of the order costs, rounded against the taker.
pub fn fill_price(o: &Value, cents: i64) -> u64 {
    let (total, dgb) = (o["cents"].as_i64().unwrap_or(1).max(1) as u128, o["dgb"].as_u64().unwrap_or(0) as u128);
    let n = dgb * cents as u128;
    let taker_buys_dollars = o["side"] == "sell";
    (if taker_buys_dollars { n.div_ceil(total) } else { n / total }) as u64
}

/// Whether the maker's `asked` price keeps to the `agreed` one: a taker buying dollars pays no more DGB, one selling
/// dollars gets no less.
pub fn price_ok(taker_buys: bool, asked: u64, agreed: u64) -> bool {
    if taker_buys { asked <= agreed } else { asked >= agreed }
}

/// Whether `cents` may be filled from what is left: $1 or more, at most what is left, and leaving $1 or more
/// (or nothing).
pub fn fill_ok(o: &Value, cents: i64) -> Result<(), String> {
    let left = o["left"].as_i64().unwrap_or(0);
    if cents < LEAST {
        return Err("a DigiDollar fill is $1 or more".into());
    }
    if cents % 100 != 0 {
        return Err("DigiDollars trade in whole dollars".into());
    }
    if cents > left {
        return Err(format!("that order has ${:.2} left", left as f64 / 100.0));
    }
    let rest = left - cents;
    if rest > 0 && rest < LEAST {
        return Err(format!("that would leave ${:.2}, under $1: take all ${:.2} or at most ${:.2}", rest as f64 / 100.0, left as f64 / 100.0, (left - LEAST) as f64 / 100.0));
    }
    Ok(())
}

/// Place an order (after the person approved it): this daemon will fill it by itself until it is
/// filled, cancelled or expires.
pub fn place(node: &Node, chain: &str, side: &str, cents: i64, dgb: u64, expires: Option<u64>) -> Result<Value, String> {
    let p = dd_chain(chain).ok_or("DigiDollar trades on dgb or dgbtest")?;
    if !(LEAST..=arcade_wallet::dd::MOST_OUTPUT).contains(&cents) {
        return Err("a DigiDollar order is $1 to $100,000".into());
    }
    // DigiDollars are not divisible here: whole dollars only (Martin, 2026-11-10).
    if cents % 100 != 0 {
        return Err("DigiDollars trade in whole dollars".into());
    }
    if dgb < arcade_wallet::dd::DUST {
        return Err("that price is under the dust line".into());
    }
    // The account placing it: the daemon fills it from that account's coins whichever account is active later.
    let account = node.app.active_account();
    let (_, held, coins, _, _) = crate::ddsvc::trade_coins(node, p)?;
    if side == "sell" {
        let have: i64 = held.iter().map(|h| h.cents).sum();
        if have < cents {
            return Err(format!("this wallet holds ${:.2} in DigiDollars", have as f64 / 100.0));
        }
    } else if coins.iter().map(|c| c.value).sum::<u64>() < dgb {
        return Err("this wallet doesn't hold that much DGB".into());
    }
    let id = arcade_mesh::random::hex(8);
    let o = json!({"id": id, "chain": p.key, "side": side, "cents": cents, "left": cents, "dgb": dgb, "status": "open",
                   "created": arcade_app::now(), "expires": expires, "fills": [], "node": node.id_hex(), "account": account});
    put(node, &o);
    let _ = node.app.event("wallet", &format!("a DigiDollar order: {} ${:.2} for {} DGB", side, cents as f64 / 100.0, dgb as f64 / 1e8));
    Ok(o)
}

pub fn cancel(node: &Node, id: &str) -> Result<Value, String> {
    let mut o = get(node, id).ok_or("no such DigiDollar order here")?;
    o["status"] = json!("cancelled");
    put(node, &o);
    Ok(o)
}

// ---------------- the maker's side: answering over the mesh ----------------

struct Pending {
    order: String,
    /// The arcade that asked (the link's node id): a new ask from it for the same order replaces the fill laid out
    /// before (the card's preview, then its Approve).
    peer: String,
    /// When that arcade's run of fills of this order began (a replaced fill's `since` carries over).
    since: u64,
    /// The key of the order's account, which the maker's side is laid out with and signs with.
    maker_key: arcade_wallet::SecretKey,
    cents: i64,
    price: u64,
    unsigned: Unsigned,
    deal: Deal,
    maker_role: Role,
    maker_mine: Vec<Utxo>,
    maker_change: Vec<u8>,
    until: u64,
}

fn pending() -> std::sync::MutexGuard<'static, HashMap<String, Pending>> {
    static P: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    P.get_or_init(Default::default).lock().unwrap()
}

/// Arcades whose fill of an order ran out unsigned: (peer, order) -> when that arcade may hold the order again.
fn resting() -> std::sync::MutexGuard<'static, HashMap<(String, String), u64>> {
    static R: OnceLock<Mutex<HashMap<(String, String), u64>>> = OnceLock::new();
    R.get_or_init(Default::default).lock().unwrap()
}

/// Drop the fills whose time ran out: the arcade that left one unsigned rests from that order for FILL_LIFE.
fn expire(p: &mut HashMap<String, Pending>, now: u64) {
    let gone: Vec<String> = p.iter().filter(|(_, f)| f.until <= now).map(|(k, _)| k.clone()).collect();
    let mut r = resting();
    r.retain(|_, until| *until > now);
    for k in gone {
        if let Some(f) = p.remove(&k) {
            r.insert((f.peer, f.order), f.until + FILL_LIFE);
        }
    }
}

/// The hold a new fill by one arcade gets at `now`: (since, until). `prev` is that arcade's live fill of the same
/// order (since, until), which the new one replaces: a run of fills never holds the order past since + FILL_LIFE +
/// SIGN_SECS. `others`: that arcade's live fills of other orders. `rest`: when it may hold this order again, if its
/// last fill of it ran out unsigned.
fn hold_for(prev: Option<(u64, u64)>, others: usize, rest: Option<u64>, now: u64) -> Result<(u64, u64), String> {
    if let Some(r) = rest.filter(|r| *r > now) {
        return Err(format!("your last fill of that order ran out unsigned; it is open to others for {} more seconds", r - now));
    }
    if others >= FILLS_PER_PEER {
        return Err(format!("your arcade has {others} fills laid out here already: sign one or let it run out first"));
    }
    Ok(match prev {
        Some((since, until)) => (since, until.max((now + SIGN_SECS).min(since + FILL_LIFE + SIGN_SECS))),
        None => (now, now + FILL_LIFE),
    })
}

fn utxo_json(u: &Utxo) -> Value {
    json!({"txid": u.outpoint.txid.to_hex(), "vout": u.outpoint.vout, "value": u.value, "script": hex::encode(&u.script)})
}

fn utxo_of(v: &Value) -> Option<Utxo> {
    Some(Utxo {
        outpoint: OutPoint { txid: Hash256::from_hex(v["txid"].as_str()?).ok()?, vout: v["vout"].as_u64()? as u32 },
        value: v["value"].as_u64()?,
        script: hex::decode(v["script"].as_str()?).ok()?,
    })
}

pub fn unsigned_json(u: &Unsigned) -> Value {
    json!({"utxos": u.utxos.iter().map(utxo_json).collect::<Vec<_>>(),
           "outs": u.outs.iter().map(|o| json!({"value": o.value, "script": hex::encode(&o.script)})).collect::<Vec<_>>(), "fee": u.fee})
}

fn unsigned_of(v: &Value) -> Option<Unsigned> {
    Some(Unsigned {
        utxos: v["utxos"].as_array()?.iter().map(utxo_of).collect::<Option<Vec<_>>>()?,
        outs: v["outs"].as_array()?.iter().map(|o| Some(TxOutSpec { value: o["value"].as_u64()?, script: hex::decode(o["script"].as_str()?).ok()? })).collect::<Option<Vec<_>>>()?,
        fee: v["fee"].as_u64()?,
    })
}

fn part_json(p: &Part) -> Value {
    json!(p.iter().map(|(i, s, w)| json!([i, hex::encode(s), w.iter().map(hex::encode).collect::<Vec<_>>()])).collect::<Vec<_>>())
}

fn part_of(v: &Value) -> Option<Part> {
    v.as_array()?
        .iter()
        .map(|x| Some((x[0].as_u64()? as usize, hex::decode(x[1].as_str()?).ok()?, x[2].as_array()?.iter().map(|w| hex::decode(w.as_str()?).ok()).collect::<Option<Vec<_>>>()?)))
        .collect()
}

/// The dollars a coin holds, read from its transaction's record on this arcade's chain.
fn cents_of(node: &Node, p: &ChainParams, op: &OutPoint) -> i64 {
    let Ok(Some((raw, _))) = node.chain.tx_raw(p, &op.txid) else { return 0 };
    let Ok(tx) = Tx::parse_with(&raw, &p.format) else { return 0 };
    arcade_ledger::digidollar::read(&tx).map(|d| d.outputs.iter().filter(|o| o.0 == op.vout).map(|o| o.2).sum()).unwrap_or(0)
}

/// A mesh question about this arcade's DigiDollar orders (shares.rs `mk`) from arcade `peer` (the link's node id;
/// this arcade's own for its own asks).
pub fn answer(node: &Node, peer: &str, arg: &str) -> Result<Value, String> {
    let q: Value = serde_json::from_str(arg).map_err(|_| "bad question".to_string())?;
    let chain = q["chain"].as_str().unwrap_or("");
    let p = dd_chain(chain).ok_or("not a DigiDollar chain")?;
    if q["op"].as_str().is_some_and(|o| o.starts_with('s')) {
        return crate::swaps::answer(node, peer, &q); // any leg for any leg (swaps.rs)
    }
    match q["op"].as_str().unwrap_or("") {
        "book" => Ok(json!({"node": node.id_hex(), "orders": orders(node, Some(p.key), false).iter().map(public).collect::<Vec<_>>()})),
        "fill" => lay_out(node, p, peer, &q),
        "sign" => finish(node, p, &q),
        _ => Err("unknown market question".into()),
    }
}

/// What a book shows of an order.
pub fn public(o: &Value) -> Value {
    json!({"order": o["id"], "chain": o["chain"], "side": o["side"], "cents": o["cents"], "left": o["left"], "dgb": o["dgb"],
           "expires": o["expires"], "node": o["node"], "min": LEAST})
}

fn lay_out(node: &Node, p: &'static ChainParams, peer: &str, q: &Value) -> Result<Value, String> {
    let id = q["order"].as_str().unwrap_or("");
    let o = get(node, id).filter(|o| o["status"] == "open" && o["chain"] == p.key).ok_or("that order is not open here")?;
    let cents = q["cents"].as_i64().ok_or("give cents")?;
    if peer.is_empty() {
        return Err("a fill is laid out for an arcade on a link".into());
    }
    // "hold": false (a card's preview): laid out to be shown, nothing held for it (an Approve asks again and signs).
    let holds = q["hold"] != false;
    let now = arcade_app::now();
    // Fills are held per asking arcade: its earlier fill of this order is replaced, it has a few at most, and one whose
    // fill ran out unsigned rests from that order a while (hold_for).
    let (since, until, laid, busy) = {
        let mut pend = pending();
        expire(&mut pend, now);
        let prev = pend.values().find(|f| f.order == id && f.peer == peer).map(|f| (f.since, f.until));
        let others = pend.values().filter(|f| f.peer == peer && f.order != id).count();
        let rest = resting().get(&(peer.to_string(), id.to_string())).copied();
        let (since, until) = if holds { hold_for(prev, others, rest, now)? } else { (now, now) };
        if holds {
            pend.retain(|_, f| !(f.order == id && f.peer == peer));
        }
        // What other fills already laid out hold of it, and every coin they spend (either side's).
        let laid: i64 = pend.values().filter(|f| f.order == id && !(f.peer == peer)).map(|f| f.cents).sum();
        let busy: Vec<OutPoint> = pend.values().filter(|f| !(f.order == id && f.peer == peer)).flat_map(|f| f.unsigned.utxos.iter().map(|u| u.outpoint)).collect();
        (since, until, laid, busy)
    };
    let mut free = o.clone();
    free["left"] = json!(o["left"].as_i64().unwrap_or(0) - laid);
    fill_ok(&free, cents)?;
    let price = fill_price(&o, cents);
    // The order's account's coins and key, whichever account is active now.
    let maker_key = crate::swaps::account_key(node, p, &crate::swaps::order_account(node, &o))?.0;
    let (_, held, dgb, dd_script, change) = crate::ddsvc::trade_coins_for(node, p, Some(&maker_key))?;
    let held: Vec<Held> = held.into_iter().filter(|h| !busy.contains(&h.coin.outpoint)).collect();
    let dgb: Vec<Utxo> = dgb.into_iter().filter(|u| !busy.contains(&u.outpoint)).collect();
    let t = &q["taker"];
    let t_dgb: Vec<Utxo> = t["dgb"].as_array().map(|a| a.iter().filter_map(utxo_of).collect()).unwrap_or_default();
    let t_change = hex::decode(t["dgb_change"].as_str().unwrap_or("")).map_err(|_| "bad change script")?;
    let t_dd_script = hex::decode(t["dd_script"].as_str().unwrap_or("")).map_err(|_| "bad DigiDollar script")?;
    if t_dd_script.len() != 34 || t_dd_script[0] != 0x51 {
        return Err("the taker's DigiDollar output is not a Taproot one".into());
    }
    // The taker's DGB is real: each coin unspent on this arcade's copy as the question says, none in another fill.
    // A hold is then backed by coins that could pay for it, never by coins made up in the question.
    for u in &t_dgb {
        let real = node.chain.coin(p, &u.outpoint).ok().flatten().is_some_and(|c| c.value == u.value && c.script == u.script);
        if !real || node.chain.mempool_spender(p.chain_no, &u.outpoint).ok().flatten().is_some() || busy.contains(&u.outpoint) {
            return Err("the taker's DGB coins are not all unspent here (or another fill holds them)".into());
        }
    }
    let maker_sells = o["side"] == "sell";
    let (seller, buyer, deal, maker_role) = if maker_sells {
        (Side { dd: held, dgb: vec![], dgb_change: change.clone() },
         Side { dd: vec![], dgb: t_dgb, dgb_change: t_change },
         Deal { cents, price, dollars_to: t_dd_script, seller_dd: dd_script.clone(), price_to: change.clone() }, Role::Seller)
    } else {
        // The taker sells dollars: its coins' dollars are read here, never taken from the question; only its coins
        // unspent on this arcade's copy, none in another fill.
        let unspent: Vec<OutPoint> = node.chain.coins_for_script(p, &t_dd_script).map_err(|e| e.to_string())?.into_iter().map(|c| c.0).collect();
        let t_dd: Vec<Held> = t["dd"].as_array().map(|a| a.iter().filter_map(utxo_of).filter(|u| unspent.contains(&u.outpoint) && !busy.contains(&u.outpoint)).map(|u| { let c = cents_of(node, p, &u.outpoint); Held { coin: u, cents: c } }).filter(|h| h.cents > 0).collect()).unwrap_or_default();
        (Side { dd: t_dd, dgb: t_dgb.clone(), dgb_change: t_change.clone() },
         Side { dd: vec![], dgb, dgb_change: change.clone() },
         Deal { cents, price, dollars_to: dd_script.clone(), seller_dd: t_dd_script, price_to: t_change }, Role::Buyer)
    };
    // The taker pays the network fee.
    let fee_by = if maker_sells { Role::Buyer } else { Role::Seller };
    let ours: Vec<OutPoint> = if maker_sells { seller.dd.iter().map(|h| h.coin.outpoint).chain(seller.dgb.iter().map(|u| u.outpoint)).collect() } else { buyer.dgb.iter().map(|u| u.outpoint).collect() };
    let u = ddtrade::layout(&seller, &buyer, &deal, fee_by)?;
    let maker_mine: Vec<Utxo> = u.utxos.iter().filter(|x| ours.contains(&x.outpoint)).cloned().collect();
    ddtrade::check(p, &u, &deal, maker_role, &maker_mine, &change, fee_by)?;
    let fill = arcade_mesh::random::hex(8);
    let out = json!({"fill": fill, "order": id, "cents": cents, "price": price, "unsigned": unsigned_json(&u),
                     "deal": {"dollars_to": hex::encode(&deal.dollars_to), "seller_dd": hex::encode(&deal.seller_dd), "price_to": hex::encode(&deal.price_to)},
                     "maker_side": o["side"], "fee_by": if fee_by == Role::Buyer { "buyer" } else { "seller" }});
    if holds {
        pending().insert(fill, Pending { order: id.into(), peer: peer.into(), since, maker_key, cents, price, unsigned: u, deal, maker_role, maker_mine, maker_change: change, until });
    }
    Ok(out)
}

fn finish(node: &Node, p: &'static ChainParams, q: &Value) -> Result<Value, String> {
    let fid = q["fill"].as_str().unwrap_or("");
    let f = {
        let mut pend = pending();
        expire(&mut pend, arcade_app::now());
        pend.remove(fid).ok_or("that fill timed out; ask again")?
    };
    let theirs = part_of(&q["part"]).ok_or("bad signatures")?;
    // Signed last, after the whole transaction is checked again as this side's.
    let fee_by = if f.maker_role == Role::Seller { Role::Buyer } else { Role::Seller };
    ddtrade::check(p, &f.unsigned, &f.deal, f.maker_role, &f.maker_mine, &f.maker_change, fee_by)?;
    if theirs.iter().any(|(i, _, _)| f.maker_mine.iter().any(|m| f.unsigned.utxos.get(*i).is_some_and(|x| x.outpoint == m.outpoint))) {
        return Err("those signatures name this arcade's own coins".into());
    }
    // Signed with the key the fill was laid out with: the order's account's, whichever account is active now.
    let mine = ddtrade::sign_mine(p, &f.unsigned, &f.maker_key)?;
    let tx = ddtrade::assemble(p, &f.unsigned, &[&theirs, &mine])?;
    let mut o = get(node, &f.order).ok_or("the order is gone")?;
    let left = o["left"].as_i64().unwrap_or(0);
    if f.cents > left || o["status"] != "open" {
        return Err("that order changed while the fill was signed".into());
    }
    crate::sitesvc::broadcast_own(node, &json!({"hex": hex::encode(&tx.raw), "chain": p.key}))?;
    let txid = tx.txid.to_hex();
    o["left"] = json!(left - f.cents);
    if left - f.cents == 0 {
        o["status"] = json!("filled");
    }
    o["fills"].as_array_mut().map(|a| a.push(json!({"txid": txid, "cents": f.cents, "dgb": f.price, "at": arcade_app::now()})));
    put(node, &o);
    let _ = node.app.event("wallet", &format!("DigiDollar order {}: ${:.2} filled for {} DGB in {txid}", f.order, f.cents as f64 / 100.0, f.price as f64 / 1e8));
    Ok(json!({"txid": txid, "hex": hex::encode(&tx.raw)}))
}

// ---------------- the taker's side ----------------

/// The DigiDollar book on a chain: this arcade's own orders and every linked arcade's.
/// Linked arcades' DigiDollar orders as each last answered: (peer, chain) -> (when, orders).
static PEER_BOOKS: std::sync::LazyLock<std::sync::Mutex<HashMap<(String, &'static str), (u64, Vec<Value>)>>> =
    std::sync::LazyLock::new(Default::default);
/// Peers asked now, per chain (an ask waits up to 30 s): never two asks of one peer at once.
static ASKING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<(String, &'static str)>>> =
    std::sync::LazyLock::new(Default::default);
/// How long the book waits for linked arcades before answering with what it has (as swaps::book).
const BOOK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
/// A peer's last answer is shown this long while it doesn't answer again.
const BOOK_KEEP: u64 = 600;

/// The DigiDollar orders here and on every linked arcade. Linked arcades are asked all at once and the book answers
/// within BOOK_WAIT with each one's newest answer. 2026-11-13: asked one after another, each up to 30 s, the JB20B's
/// dozen links held this (and with it /r/market/markets, which counts these orders) for minutes, so the Swap app in
/// the desktop's Apps tab drew nothing at all.
pub fn book(node: &Node, chain: &str) -> Result<Vec<Value>, String> {
    let p = dd_chain(chain).ok_or("DigiDollar trades on dgb or dgbtest")?;
    let mut all: Vec<Value> = orders(node, Some(p.key), false).iter().map(|o| { let mut v = public(o); v["mine"] = json!(true); v }).collect();
    let peers = crate::shares::linked_ids(node);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let mut asked = 0;
    for peer in &peers {
        if !ASKING.lock().unwrap().insert((peer.clone(), p.key)) {
            continue;
        }
        asked += 1;
        let (peer, tx, net, key) = (peer.clone(), tx.clone(), p.net, p.key);
        std::thread::spawn(move || {
            let q = json!({"what": "mk", "net": net, "arg": json!({"op": "book", "chain": key}).to_string()});
            if let Ok(a) = crate::shares::ask_linked(&peer, q) {
                PEER_BOOKS.lock().unwrap().insert((peer.clone(), key), (arcade_app::now(), a["orders"].as_array().cloned().unwrap_or_default()));
            }
            ASKING.lock().unwrap().remove(&(peer, key));
            let _ = tx.send(());
        });
    }
    drop(tx);
    let until = std::time::Instant::now() + BOOK_WAIT;
    for _ in 0..asked {
        if rx.recv_timeout(until.saturating_duration_since(std::time::Instant::now())).is_err() {
            break;
        }
    }
    let books = PEER_BOOKS.lock().unwrap();
    for peer in &peers {
        if let Some((_, orders)) = books.get(&(peer.clone(), p.key)).filter(|(at, _)| arcade_app::now().saturating_sub(*at) <= BOOK_KEEP) {
            for o in orders {
                let mut o = o.clone();
                o["node"] = json!(peer);
                all.push(o);
            }
        }
    }
    Ok(all)
}

/// Ask the maker to lay out a fill of `cents` of its order, with this wallet's coins as the taker's side,
/// and check the answer as this side's own: (the answer, the checked transaction, the deal, this key).
/// `price`: the DGB this side agreed to (the card's): a buyer of dollars pays no more, a seller gets no less.
/// `hold`: whether the maker holds its coins for this fill (an Approve, signed next); a card's preview holds nothing.
pub fn ask_fill(node: &Node, chain: &str, maker: &str, order: &str, cents: i64, price: u64, hold: bool) -> Result<(Value, Unsigned, Deal, Role, Vec<Utxo>, Vec<u8>), String> {
    let p = dd_chain(chain).ok_or("DigiDollar trades on dgb or dgbtest")?;
    let (_, held, dgb, dd_script, change) = crate::ddsvc::trade_coins(node, p)?;
    let taker = json!({"dgb": dgb.iter().map(utxo_json).collect::<Vec<_>>(), "dgb_change": hex::encode(&change),
                       "dd_script": hex::encode(&dd_script), "dd": held.iter().map(|h| utxo_json(&h.coin)).collect::<Vec<_>>()});
    let q = json!({"op": "fill", "chain": p.key, "order": order, "cents": cents, "taker": taker, "hold": hold});
    let a = if maker == node.id_hex() { answer(node, maker, &q.to_string())? } else {
        crate::shares::ask_linked(maker, json!({"what": "mk", "net": p.net, "arg": q.to_string()}))?
    };
    let u = unsigned_of(&a["unsigned"]).ok_or("the maker's answer doesn't read")?;
    let d = &a["deal"];
    let deal = Deal {
        cents: a["cents"].as_i64().ok_or("no cents")?,
        price: a["price"].as_u64().ok_or("no price")?,
        dollars_to: hex::decode(d["dollars_to"].as_str().unwrap_or("")).map_err(|e| e.to_string())?,
        seller_dd: hex::decode(d["seller_dd"].as_str().unwrap_or("")).map_err(|e| e.to_string())?,
        price_to: hex::decode(d["price_to"].as_str().unwrap_or("")).map_err(|e| e.to_string())?,
    };
    // This side's own terms, not the maker's word: the cents asked for, and where its dollars or DGB go.
    if deal.cents != cents {
        return Err("the maker laid out a different amount".into());
    }
    let taker_buys = a["maker_side"] == "sell";
    let role = if taker_buys { Role::Buyer } else { Role::Seller };
    // The price is this side's too, not only the maker's word: the checks below measure the transaction against
    // deal.price, so a maker that names another price at the Approve is refused here.
    if !price_ok(taker_buys, deal.price, price) {
        return Err(format!("the maker asks a different price now ({} DGB satoshis where you agreed to {price}); ask again", deal.price));
    }
    if taker_buys && deal.dollars_to != dd_script {
        return Err("the maker would send the dollars somewhere else".into());
    }
    if !taker_buys && (deal.price_to != change || deal.seller_dd != dd_script) {
        return Err("the maker would pay the DGB somewhere else".into());
    }
    let mine: Vec<Utxo> = u.utxos.iter().filter(|x| dgb.iter().any(|c| c.outpoint == x.outpoint) || held.iter().any(|h| h.coin.outpoint == x.outpoint)).cloned().collect();
    let fee_by = if taker_buys { Role::Buyer } else { Role::Seller };
    ddtrade::check(p, &u, &deal, role, &mine, &change, fee_by)?;
    Ok((a, u, deal, role, mine, change))
}

/// The taker signs its inputs and hands them to the maker, who signs last and broadcasts: the sent tx.
pub fn take(node: &Node, chain: &str, maker: &str, order: &str, cents: i64, price: u64) -> Result<(Tx, u64), String> {
    let p = dd_chain(chain).ok_or("DigiDollar trades on dgb or dgbtest")?;
    let (a, u, ..) = ask_fill(node, chain, maker, order, cents, price, true)?;
    let (sk, ..) = crate::ddsvc::trade_coins(node, p)?;
    let part = ddtrade::sign_mine(p, &u, &sk)?;
    let q = json!({"op": "sign", "chain": p.key, "fill": a["fill"], "part": part_json(&part)});
    let done = if maker == node.id_hex() { answer(node, maker, &q.to_string())? } else {
        crate::shares::ask_linked(maker, json!({"what": "mk", "net": p.net, "arg": q.to_string()}))?
    };
    let tx = Tx::parse_with(&hex::decode(done["hex"].as_str().unwrap_or("")).map_err(|e| e.to_string())?, &p.format).map_err(|e| e.to_string())?;
    // The transaction sent is the one this side checked and signed.
    if tx.outputs.len() != u.outs.len() || tx.outputs.iter().zip(&u.outs).any(|(a, b)| a.value != b.value || a.script_pubkey != b.script) {
        return Err("the maker sent a different transaction".into());
    }
    Ok((tx, u.fee))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The maker's price at the Approve must keep to the card's: a buyer of dollars never pays more DGB, a seller of
    /// dollars never gets less; a better price for this side goes ahead.
    #[test]
    fn the_approved_price_holds_at_the_take() {
        let agreed = 1_000_000;
        assert!(price_ok(true, agreed, agreed));
        assert!(price_ok(true, agreed - 1, agreed));
        assert!(!price_ok(true, agreed + 1, agreed));
        assert!(!price_ok(true, u64::MAX, agreed));
        assert!(price_ok(false, agreed, agreed));
        assert!(price_ok(false, agreed + 1, agreed));
        assert!(!price_ok(false, agreed - 1, agreed));
        assert!(!price_ok(false, 0, agreed));
    }

    /// The pinned price is the book's fill price, rounded against the taker either way.
    #[test]
    fn the_pinned_price_is_the_books() {
        let sell = json!({"side": "sell", "cents": 300, "dgb": 1000});
        let buy = json!({"side": "buy", "cents": 300, "dgb": 1000});
        assert_eq!(fill_price(&sell, 100), 334); // a taker buying dollars pays the rounding
        assert_eq!(fill_price(&buy, 100), 333); // a taker selling dollars gets it rounded down
    }
}

#[cfg(test)]
mod hold_tests {
    use super::*;

    /// A fill holds for FILL_LIFE; the same arcade asking again replaces it without holding much longer (SIGN_SECS
    /// for its signatures, never past since + FILL_LIFE + SIGN_SECS); a few per arcade; a rest after one ran out.
    #[test]
    fn fill_holds_are_bounded_per_arcade_and_in_time() {
        assert_eq!(hold_for(None, 0, None, 1000), Ok((1000, 1000 + FILL_LIFE)));
        assert_eq!(hold_for(Some((1000, 1600)), 0, None, 1100), Ok((1000, 1600)), "asking again holds no longer");
        assert_eq!(hold_for(Some((1000, 1600)), 0, None, 1590), Ok((1000, 1710)), "time to sign an Approve");
        assert_eq!(hold_for(Some((1000, 1710)), 0, None, 1700), Ok((1000, 1720)), "never past since + FILL_LIFE + SIGN_SECS");
        assert_eq!(hold_for(Some((1000, 1720)), 0, None, 1719), Ok((1000, 1720)));
        assert!(hold_for(None, FILLS_PER_PEER, None, 1000).unwrap_err().contains("fills laid out"));
        assert!(hold_for(None, 0, Some(1500), 1000).unwrap_err().contains("ran out unsigned"));
        assert!(hold_for(None, 0, Some(1000), 1000).is_ok(), "the rest is over");
    }

    /// A fill that ran out unsigned is dropped and its arcade rests from that order for FILL_LIFE.
    #[test]
    fn a_fill_left_unsigned_rests_its_arcade() {
        let f = |peer: &str, until: u64| Pending {
            order: "o1".into(), peer: peer.into(), since: until - FILL_LIFE, maker_key: arcade_wallet::SecretKey::from_slice(&[1; 32]).unwrap(),
            cents: 100, price: 1, unsigned: Unsigned { utxos: vec![], outs: vec![], fee: 0 },
            deal: Deal { cents: 100, price: 1, dollars_to: vec![], seller_dd: vec![], price_to: vec![] }, maker_role: Role::Seller,
            maker_mine: vec![], maker_change: vec![], until,
        };
        let mut p = HashMap::new();
        p.insert("f1".to_string(), f("hold-test-a", 5000));
        p.insert("f2".to_string(), f("hold-test-b", 9000));
        expire(&mut p, 5000);
        assert!(!p.contains_key("f1") && p.contains_key("f2"));
        assert_eq!(resting().get(&("hold-test-a".to_string(), "o1".to_string())).copied(), Some(5000 + FILL_LIFE));
        assert!(!resting().contains_key(&("hold-test-b".to_string(), "o1".to_string())));
    }
}
