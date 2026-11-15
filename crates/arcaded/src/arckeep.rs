//! Time keepers (docs/ARCADECHAIN.md §6.2, §14.23).
//!
//! Every candidate for a height needs a proof of time from the same challenge (the block before), each to its own
//! count. A **time keeper** runs that calculation once: to the lowest candidate's count, hands that candidate its
//! proof, runs on from there to the next candidate's count, and so on. The k-th candidate's proof is the first k runs
//! (arcade-arc `time::join`), allowed from rules version 2 (up to `params::MAX_POT_RUNS`).
//!
//! Over mesh links, as "ac" frames, each passed on once to every link (like waiting storage items):
//!
//! ```text
//! cand {g, prev, w, writer, vrf}            a writer's draw for the block after `prev` (weight, key, VRF proof)
//! tp   {g, prev, steps, ol} + blob          a proof of time of `steps` from `prev`: output (ol bytes) then proof
//! ```
//!
//! Nothing depends on a keeper. A writer whose score could be the height's best (§6.2: under 3 times the best one
//! expected from the whole network's weight, about 19 heights in 20) says so with a `cand` and also runs its own proof
//! as before; it writes with whichever proof is ready first. A writer that can't be the best doesn't announce and runs
//! its own proof as before, at normal priority while it is for the tip (at the lowest priority a busy machine never
//! finished it, and with few writers every draw can look unlikely: the testnet stalled so at block 1,061).
//! Being a keeper costs a core; an arcade turns it on with `arc.keeper {on: true}`.

use crate::arcsvc::{self, Clock};
use arcade_arc::block::{Key, TimeProof, VrfProof};
use arcade_arc::params;
use arcade_arc::time::{self, ChiaVdf, Segment, TimeProofs};
use arcade_chain::hash::Hash256;
use arcade_mesh::link::{Frame, Link, LinkError};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// A writer's draw, checked against the chain: what its proof needs.
#[derive(Clone)]
struct Cand {
    steps: u64,
}

#[derive(Default)]
struct State {
    /// The tip everything here is for (the challenge is the block after it).
    tip: Option<Hash256>,
    cands: Vec<Cand>,
    /// Proofs of time for the tip, by steps: made here or passed on.
    proofs: HashMap<u64, TimeProof>,
    /// Frames already taken in (by what they say), so each is passed on once.
    seen: HashSet<Vec<u8>>,
    /// Frames to pass on, numbered; each link is sent the ones after the last it got.
    out: VecDeque<(u64, Frame)>,
    next: u64,
    told: HashMap<String, u64>,
    /// For arc.status: proofs made here as a keeper, and taken from keepers for our own blocks.
    made: u64,
    used: u64,
}

static ST: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

/// Candidates kept per height (the lowest), frames waiting to go out.
const MAX_CANDS: usize = 64;
const MAX_OUT: usize = 256;

/// The META key of the keeper switch.
const SETTING: &[u8] = b"arc_keeper";

fn reset_for(st: &mut State, tip: Hash256) {
    if st.tip != Some(tip) {
        st.tip = Some(tip);
        st.cands.clear();
        st.proofs.clear();
        st.seen.clear();
        st.out.clear();
    }
}

fn push_out(st: &mut State, f: Frame) {
    st.next += 1;
    let n = st.next;
    st.out.push_back((n, f));
    while st.out.len() > MAX_OUT {
        st.out.pop_front();
    }
}

/// Whether a score could be the best of the height (§6.2): the best score of the whole network's weight `total` is
/// about −ln(u)/total with u uniform, so under 3/total about 19 heights in 20. A score is −log2(u)·2^96/W (arcade-arc
/// draw.rs), so the test is −log2(u)·ln 2·total/W < 3.
pub fn plausible(score: u128, total: u128) -> bool {
    const TWO_96: f64 = 79_228_162_514_264_337_593_543_950_336.0;
    total > 0 && (score as f64) * (total as f64) < 3.0 / std::f64::consts::LN_2 * TWO_96
}

#[cfg(test)]
mod tests {
    use super::plausible;

    #[test]
    fn a_score_could_win_up_to_three_times_the_best_expected() {
        // Three equal holders of 1 GiB: a holder could win when −ln(u) · 3 < 3, i.e. −log2(u) < 1/ln 2 ≈ 1.4427.
        let (w, total) = (1u128 << 30, 3u128 << 30);
        let score = |bits: f64| ((bits * 2f64.powi(32)) as u128) * (1u128 << 64) / w; // L × 2^96 / W, as draw.rs
        assert!(plausible(score(0.5), total));
        assert!(plausible(score(1.44), total));
        assert!(!plausible(score(1.45), total));
        assert!(!plausible(score(8.0), total));
        assert!(!plausible(score(0.5), 0), "no weight anywhere: nothing to measure against");
    }
}

/// The writer drew `ticket`-like values for the block after `tip`: announce it to the keepers (and to this arcade's own,
/// when it is one).
pub fn announce(f: Frame, tip: Hash256, me: Key, weight: u64, vrf: VrfProof, steps: u64) {
    let mut st = ST.lock().unwrap();
    reset_for(&mut st, tip);
    let key = [&b"cand"[..], &me, &vrf[..]].concat();
    if st.seen.insert(key) {
        let _ = (weight, vrf);
        st.cands.push(Cand { steps });
        push_out(&mut st, f);
    }
}

/// A keeper's proof for `steps` from the block after `tip`, if one came (already checked).
pub fn proof_for(tip: &Hash256, steps: u64) -> Option<TimeProof> {
    let mut st = ST.lock().unwrap();
    if st.tip != Some(*tip) {
        return None;
    }
    let p = st.proofs.get(&steps).cloned();
    if p.is_some() {
        st.used += 1;
    }
    p
}

fn hex32(v: &Value) -> Option<[u8; 32]> {
    v.as_str().and_then(|s| hex::decode(s).ok()).and_then(|b| b.try_into().ok())
}

/// A block id as `Hash256::to_hex` writes it (reversed, as Bitcoin shows hashes).
fn block_id(v: &Value) -> Option<Hash256> {
    v.as_str().and_then(|s| Hash256::from_hex(s).ok())
}

/// A `cand` frame: checked against the chain (the draw, the weight, a score that could win), kept, passed on.
pub fn on_cand(f: &Frame) {
    let Some(svc) = arcsvc::svc() else { return };
    let (Some(tip), Some(writer), Some(weight)) = (block_id(&f.head["prev"]), hex32(&f.head["writer"]), f.head["w"].as_u64()) else { return };
    let Some(vrf) = f.head["vrf"].as_str().and_then(|s| hex::decode(s).ok()).and_then(|b| <VrfProof>::try_from(b).ok()) else { return };
    let key = [&b"cand"[..], &writer, &vrf[..]].concat();
    {
        let st = ST.lock().unwrap();
        if st.tip == Some(tip) && st.seen.contains(&key) {
            return;
        }
    }
    let Some((score, steps, total)) = svc.with_chain(|c| {
        if c.tip().id != tip {
            return None;
        }
        let seed = arcsvc::seed_proof(c);
        let (score, steps) = c.candidate(&writer, weight, &vrf, seed.as_ref())?;
        Some((score, steps, c.storage().total_weight()))
    }) else {
        return;
    };
    if !plausible(score, total) {
        return; // a score that can't win isn't worth anyone's time
    }
    let mut st = ST.lock().unwrap();
    reset_for(&mut st, tip);
    if !st.seen.insert(key) || st.cands.len() >= MAX_CANDS {
        return;
    }
    st.cands.push(Cand { steps });
    push_out(&mut st, f.clone());
}

/// A `tp` frame: a proof for a candidate's count on the tip, checked, kept, passed on.
pub fn on_tp(f: &Frame) {
    let Some(svc) = arcsvc::svc() else { return };
    let (Some(tip), Some(steps), Some(ol)) = (block_id(&f.head["prev"]), f.head["steps"].as_u64(), f.head["ol"].as_u64()) else { return };
    let ol = ol as usize;
    if ol > f.blob.len() || f.blob.len() > arcade_arc::block::MAX_TIME_BYTES * 2 {
        return;
    }
    let key = [&b"tp"[..], &tip.0, &steps.to_le_bytes()].concat();
    {
        // Only for a count some candidate needs, once: a proof costs its maker real time, its check costs ours.
        let st = ST.lock().unwrap();
        if st.tip != Some(tip) || st.seen.contains(&key) || !st.cands.iter().any(|c| c.steps == steps) {
            return;
        }
    }
    let proof = TimeProof { output: f.blob[..ol].to_vec(), proof: f.blob[ol..].to_vec() };
    let good = svc.with_chain(|c| {
        let t = c.tip();
        t.id == tip && c.proofs().verify_pot(&time::block_challenge(c.net(), &tip), steps, &proof, params::pot_runs(c.net(), t.height + 1))
    });
    if !good {
        return;
    }
    let mut st = ST.lock().unwrap();
    if st.tip != Some(tip) || !st.seen.insert(key) {
        return;
    }
    st.proofs.insert(steps, proof);
    push_out(&mut st, f.clone());
}

/// The frames this link hasn't been sent yet.
pub fn tick(peer: &str, link: &mut Link) -> Result<(), LinkError> {
    let due: Vec<Frame> = {
        let mut st = ST.lock().unwrap();
        let last = st.told.get(peer).copied().unwrap_or(0);
        let due: Vec<Frame> = st.out.iter().filter(|(n, _)| *n > last).map(|(_, f)| f.clone()).collect();
        let next = st.next;
        st.told.insert(peer.to_string(), next);
        due
    };
    for f in &due {
        link.send(f)?;
    }
    Ok(())
}

pub fn on(node: &crate::node::Node) -> bool {
    node.app.store.read(|r| r.get(arcade_app::META, SETTING)).ok().flatten().is_some_and(|v| v == b"1")
}

/// `arc.keeper {on?}`: turn this arcade's time keeper on or off (it starts or stops within a block); its state.
pub fn api(node: &crate::node::Node, req: &Value) -> Result<Value, String> {
    if let Some(on) = req["on"].as_bool() {
        node.app.store.write(|w| w.put(arcade_app::META, SETTING, if on { b"1" } else { b"0" })).map_err(|e| e.to_string())?;
    }
    Ok(status(node))
}

pub fn status(node: &crate::node::Node) -> Value {
    let st = ST.lock().unwrap();
    let mut steps: Vec<u64> = st.cands.iter().map(|c| c.steps).collect();
    steps.sort_unstable();
    json!({"on": on(node), "candidates": steps, "proofs_here": st.proofs.len(), "made": st.made, "used_for_our_blocks": st.used})
}

/// The keeper: while it is on and the chain runs the real proof of time, one calculation per tip, cut at each
/// candidate's count (the lowest first, at most the runs the rules allow).
pub fn run(node: Arc<crate::node::Node>) {
    // The run in progress: (tip, its result when done).
    let job: Arc<Mutex<Option<(Hash256, Option<Segment>)>>> = Arc::new(Mutex::new(None));
    let running: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
    // Where this tip's calculation stands: (tip, steps done, its runs).
    let mut at: Option<(Hash256, u64, Vec<Segment>)> = None;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let Some(svc) = arcsvc::svc() else {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        };
        if !on(&node) {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        let Some((tip, challenge, most, vdf)) = svc.with_chain(|c| {
            let t = c.tip();
            Some((t.id, time::block_challenge(c.net(), &t.id), params::pot_runs(c.net(), t.height + 1), *c.proofs() == Clock::Vdf))
        }) else {
            continue;
        };
        if !vdf {
            std::thread::sleep(Duration::from_secs(5));
            continue; // the stand-in needs no keeper
        }
        if at.as_ref().is_none_or(|a| a.0 != tip) {
            // A new tip: whatever still runs for the old one gets spare CPU only.
            for tid in running.lock().unwrap().iter() {
                crate::rulecheck::lowest_priority_of(*tid);
            }
            at = Some((tip, 0, Vec::new()));
        }
        let (_, done, runs) = at.as_mut().expect("set above");
        // A run that finished? (One for an old tip is thrown away: its thread can't be stopped, only left behind.)
        let state = job.lock().unwrap().as_ref().map(|(t, seg)| (*t == tip, seg.is_some()));
        match state {
            Some((true, false)) => continue, // still running
            Some((true, true)) => {
                let seg = job.lock().unwrap().take().and_then(|x| x.1).expect("a finished run");
                *done += seg.steps;
                runs.push(seg);
                let proof = time::join(runs);
                let mut f = arcsvc::keeper_frame("tp", json!({"prev": tip.to_hex(), "steps": *done, "ol": proof.output.len()}));
                f.blob = [&proof.output[..], &proof.proof[..]].concat();
                let mut st = ST.lock().unwrap();
                reset_for(&mut st, tip);
                st.seen.insert([&b"tp"[..], &tip.0, &done.to_le_bytes()].concat());
                st.proofs.insert(*done, proof);
                st.made += 1;
                push_out(&mut st, f);
                eprintln!("arc: time keeper: proof of {} steps for the block after {} ({} runs)", done, &tip.to_hex()[..16], runs.len());
            }
            Some((false, _)) => *job.lock().unwrap() = None,
            None => {}
        }
        if runs.len() >= most {
            continue;
        }
        // The next count to reach: the lowest candidate's beyond where we are.
        let target = {
            let st = ST.lock().unwrap();
            if st.tip != Some(tip) {
                continue;
            }
            st.cands.iter().map(|c| c.steps).filter(|s| *s > *done).min()
        };
        let Some(target) = target else { continue };
        let from = runs.last().map(|r| r.output.clone()).unwrap_or_else(|| ChiaVdf::start().to_vec());
        let n = target - *done;
        *job.lock().unwrap() = Some((tip, None));
        let (job, running) = (job.clone(), running.clone());
        let _ = std::thread::Builder::new().name("arc time keeper".into()).spawn(move || {
            let me = crate::rulecheck::thread_id();
            running.lock().unwrap().push(me);
            let seg = ChiaVdf.run(&challenge, &from, n);
            running.lock().unwrap().retain(|t| *t != me);
            let mut j = job.lock().unwrap();
            if j.as_ref().is_some_and(|x| x.0 == tip) {
                *j = seg.map(|s| (tip, Some(s)));
            }
        });
    }
}
