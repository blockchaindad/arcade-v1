//! How far an answer from the mesh is vouched for (sadfrogltc's standing rule, 2026-11-16):
//! green = this arcade's own data, or two or more independent arcades agree; yellow = one arcade's
//! word; grey = nobody answered; red = a problem (arcades disagree, or the answer contradicts what
//! this arcade can check itself). One source is still used, shown with a caution; red is not.
//!
//! The same idea as the wallet's Trust in shares (one-source sends); kept here for what the App
//! Store, /content and operator lists show.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Green,
    Yellow,
    Grey,
    Red,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Green => "green",
            Level::Yellow => "yellow",
            Level::Grey => "grey",
            Level::Red => "red",
        }
    }

    /// From how many independent arcades back an answer, none of them disagreeing.
    pub fn of_sources(n: usize) -> Level {
        match n {
            0 => Level::Grey,
            1 => Level::Yellow,
            _ => Level::Green,
        }
    }
}

/// Ask an Explorer question of up to `most` holders of index `ix` at once, no two with the same
/// node id or the same host (crate::refill::independent, as for blocks). Every answer in within
/// `ms`: (answer, node id). The caller compares them.
pub fn ask_each(node: &Node, p: &ChainParams, ix: &str, cmd: &str, req: &Value, most: usize, ms: u64) -> Vec<(Value, String)> {
    if crate::handlegate::is_gated_read(cmd) && crate::handlegate::check_local(node).is_err() {
        return Vec::new();
    }
    // The chain's own parameters (they live for the whole run), for the threads below.
    let Some(p) = arcade_chain::params::ALL_CHAINS.iter().copied().find(|x| x.chain_no == p.chain_no).or_else(|| arcade_chain::params::regtest_by_key(p.key)) else {
        return Vec::new();
    };
    let mut holders = crate::shares::index_holders(node, p.net, ix);
    holders.sort_by_key(|(id, _)| !crate::shares::is_linked(node, id));
    let who: Vec<(String, String)> = holders.into_iter().filter_map(|(id, _)| crate::shares::holder_host(node, &id).map(|h| (id, h))).collect();
    let picked: Vec<String> = crate::refill::independent(&who).into_iter().take(most).collect();
    if picked.is_empty() {
        return Vec::new();
    }
    let (tx, rx) = std::sync::mpsc::channel();
    for id in &picked {
        let (tx, n, id, cmd, req) = (tx.clone(), node.self_arc(), id.clone(), cmd.to_string(), req.clone());
        std::thread::spawn(move || {
            let r = crate::shares::ask_explorer_at(&n, p, &id, &cmd, &req);
            let _ = tx.send((id, r));
        });
    }
    drop(tx);
    let until = Instant::now() + Duration::from_millis(ms);
    let mut out = Vec::new();
    for _ in 0..picked.len() {
        let Some(left) = until.checked_duration_since(Instant::now()) else { break };
        match rx.recv_timeout(left) {
            Ok((id, Some(Ok(v)))) => out.push((v, id)),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

/// Answers grouped by what they say (`key`, None: says nothing comparable): each distinct value with
/// the node ids that sent it, the most backed first.
pub fn tally<K: PartialEq + Clone>(answers: &[(Option<K>, String)]) -> Vec<(K, Vec<String>)> {
    let mut groups: Vec<(K, Vec<String>)> = Vec::new();
    for (k, id) in answers {
        let Some(k) = k else { continue };
        match groups.iter_mut().find(|g| g.0 == *k) {
            Some(g) if !g.1.contains(id) => g.1.push(id.clone()),
            Some(_) => {}
            None => groups.push((k.clone(), vec![id.clone()])),
        }
    }
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    groups
}

/// An event for something that turned red, once per `key` while this arcade runs (a page asking
/// again and again raises it once).
pub fn alert_once(node: &Node, key: &str, kind: &str, text: &str) {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);
    let mut g = SEEN.lock().unwrap();
    let s = g.get_or_insert_with(Default::default);
    if s.len() > 10_000 {
        s.clear();
    }
    if s.insert(key.to_string()) {
        drop(g);
        let _ = node.app.event(kind, text);
    }
}

/// A short name for a holder: its @tag, else the start of its node id.
pub fn who(id: &str) -> String {
    crate::meshview::tag_of(id).map(|t| format!("@{t}")).unwrap_or_else(|| id.chars().take(8).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_and_tally() {
        assert_eq!(Level::of_sources(0), Level::Grey);
        assert_eq!(Level::of_sources(1), Level::Yellow);
        assert_eq!(Level::of_sources(3), Level::Green);
        let a = |k: Option<&str>, id: &str| (k.map(String::from), id.to_string());
        let t = tally(&[a(Some("x"), "n1"), a(Some("y"), "n2"), a(None, "n3"), a(Some("x"), "n4"), a(Some("x"), "n1")]);
        assert_eq!(t, vec![("x".to_string(), vec!["n1".to_string(), "n4".to_string()]), ("y".to_string(), vec!["n2".to_string()])]);
        assert!(tally::<String>(&[a(None, "n1")]).is_empty());
    }
}
