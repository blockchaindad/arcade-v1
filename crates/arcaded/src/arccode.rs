//! The arcade's own code, kept by every arcade (Martin 2026-11-14: "make arcade v1 the code into the genesis block and
//! make everyone keep it": the source with its whole git history, the vendored dependencies so it rebuilds offline,
//! and the prebuilt programs).
//!
//! - **A release** is a signed record: its name (`@arcade/arcade-v1`), version, the git commit it is, the release before
//!   it, and its files by SHA-256: a git bundle of the repo (every commit up to this one: `git clone` reads it), the
//!   vendored dependencies (a reproducible tar, zstd), and a program archive per platform. The record's id is the
//!   SHA-256 of its bytes; the signature is by a release key this arcade trusts ([`RELEASE_KEYS`], or `--code-key` on
//!   closed test networks). On mainnet, block 0 names the first release's id, so the code the network started from is
//!   fixed by the chain itself.
//! - **Every arcade keeps** every release's bundle and vendored dependencies, and the programs of the newest
//!   [`KEEP_PROGRAMS`] releases, in `<home>/code/` (files by SHA-256, each checked before it is kept).
//! - **Over the mesh** ("co" frames): `r` a release record (passed on once per link); `h` the files an arcade holds;
//!   `g {id, f, at}` asks a holder for 48 kB of a file over the link it already has (so it reaches arcades behind a
//!   NAT, as borrowing does: crvia), answered `a {id, ...}`.
//! - `code.status`, `code.add` (a release made by scripts/code-publish.py, with its files), `code.export` (a release's
//!   files written out, ready for `git clone` and `cargo build --offline`).

use crate::node::Node;
use arcade_mesh::link::{Frame, Link, LinkError};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Keys whose releases every arcade takes (hex ed25519). Martin 2026-11-14: "strawberry should hold the release key":
/// Strawberry's release signing key, the one the original arcade's releases were already signed with (its public half
/// is pinned in the original arcade's release.py). It signs code releases and nothing else. Closed test networks add
/// one with `--code-key`.
pub const RELEASE_KEYS: &[&str] = &["9c955cd5c945d72775f2fa8592b13f5098285d1205edd0be2cca11591b2a008a"];
/// Releases whose programs are kept (the bundles and dependencies of every release are).
pub const KEEP_PROGRAMS: usize = 5;
/// Bytes of a file per answer (a mesh frame is 64 KiB at most, head and blob).
const CHUNK: u64 = 48_000;
/// Pieces of a file asked for at once.
const WINDOW: usize = 16;
/// Most bytes of one file (a release's dependencies are ~62 MB; room to grow).
const MAX_FILE: u64 = 1 << 30;

static KEYS: OnceLock<Vec<VerifyingKey>> = OnceLock::new();
static HOME: OnceLock<PathBuf> = OnceLock::new();

fn sha_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn message(id: &str) -> Vec<u8> {
    [&b"ARCADE code release\0"[..], id.as_bytes()].concat()
}

/// A release, as its record says.
#[derive(Clone, Debug)]
pub struct Release {
    pub id: String,
    /// The record's bytes, as signed (canonical JSON from the publisher).
    pub record: String,
    pub sig: String,
    pub name: String,
    pub version: String,
    pub commit: String,
    pub time: u64,
    /// file name -> (sha256, bytes)
    pub files: BTreeMap<String, (String, u64)>,
}

impl Release {
    /// A record and its signature, checked: its fields, and a signature by a trusted release key.
    pub fn check(record: &str, sig: &str) -> Result<Release, String> {
        let v: Value = serde_json::from_str(record).map_err(|_| "a release record must be JSON")?;
        let id = sha_hex(record.as_bytes());
        let sig = sig.to_lowercase();
        let s: [u8; 64] = hex::decode(&sig).ok().and_then(|b| b.try_into().ok()).ok_or("a release signature is 64 bytes")?;
        let keys = KEYS.get().map(Vec::as_slice).unwrap_or(&[]);
        if !keys.iter().any(|k| k.verify(&message(&id), &Signature::from_bytes(&s)).is_ok()) {
            return Err("not signed by a release key this arcade trusts".into());
        }
        let txt = |k: &str| v[k].as_str().map(String::from).ok_or(format!("a release record needs {k}"));
        let mut files = BTreeMap::new();
        for (name, f) in v["files"].as_object().ok_or("a release record needs files")? {
            let sha = f["sha256"].as_str().filter(|s| s.len() == 64 && hex::decode(s).is_ok()).ok_or("a file needs its sha256")?;
            let bytes = f["bytes"].as_u64().filter(|b| *b <= MAX_FILE).ok_or("a file needs its size (at most 1 GiB)")?;
            if name.contains('/') || name.contains("..") || name.is_empty() {
                return Err("a file name is a plain name".into());
            }
            files.insert(name.clone(), (sha.to_lowercase(), bytes));
        }
        let commit = txt("commit")?;
        if commit.len() != 40 || hex::decode(&commit).is_err() {
            return Err("commit is a git commit id".into());
        }
        Ok(Release { id, record: record.to_string(), sig, name: txt("name")?, version: txt("version")?, commit, time: v["time"].as_u64().unwrap_or(0), files })
    }

    pub fn is_program(name: &str) -> bool {
        name.starts_with("program-")
    }
}

#[derive(Default)]
struct State {
    releases: BTreeMap<String, Release>,
    /// Files this arcade holds (sha256).
    have: HashSet<String>,
    /// Per peer: release ids told, and the files it said it holds.
    told: HashMap<String, HashSet<String>>,
    peer_has: HashMap<String, HashSet<String>>,
    have_told: HashMap<String, usize>,
    /// Files being fetched now.
    fetching: HashSet<String>,
}

static ST: OnceLock<Mutex<State>> = OnceLock::new();

fn st() -> std::sync::MutexGuard<'static, State> {
    ST.get_or_init(Default::default).lock().unwrap()
}

fn dir() -> PathBuf {
    HOME.get().cloned().unwrap_or_default().join("code")
}

fn file_path(sha: &str) -> PathBuf {
    dir().join("files").join(sha)
}

/// The files to keep: every release's non-program files, and the programs of the newest KEEP_PROGRAMS of each name.
fn wanted(s: &State) -> BTreeMap<String, u64> {
    let mut by_name: BTreeMap<&str, Vec<&Release>> = BTreeMap::new();
    for r in s.releases.values() {
        by_name.entry(&r.name).or_default().push(r);
    }
    let mut out = BTreeMap::new();
    for (_, mut rs) in by_name {
        rs.sort_by(|a, b| b.time.cmp(&a.time).then(b.id.cmp(&a.id)));
        for (i, r) in rs.iter().enumerate() {
            for (name, (sha, bytes)) in &r.files {
                if !Release::is_program(name) || i < KEEP_PROGRAMS {
                    out.insert(sha.clone(), *bytes);
                }
            }
        }
    }
    out
}

/// Take a release record in (checked, saved). Whether it was new.
fn take_release(record: &str, sig: &str) -> Result<bool, String> {
    let r = Release::check(record, sig)?;
    let mut s = st();
    if s.releases.contains_key(&r.id) {
        return Ok(false);
    }
    let d = dir().join("releases");
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    std::fs::write(d.join(format!("{}.json", r.id)), json!({"record": r.record, "sig": r.sig}).to_string()).map_err(|e| e.to_string())?;
    eprintln!("arc code: release {} {} (commit {}) known", r.name, r.version, &r.commit[..12]);
    s.releases.insert(r.id.clone(), r);
    Ok(true)
}

/// Keep a file whose bytes hash to `sha`: written beside, checked, then moved in.
fn keep_file(sha: &str, src: &Path) -> Result<(), String> {
    let mut h = Sha256::new();
    let mut f = std::fs::File::open(src).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    if hex::encode(h.finalize()) != sha {
        return Err(format!("{} isn't the file {sha}", src.display()));
    }
    std::fs::create_dir_all(dir().join("files")).map_err(|e| e.to_string())?;
    std::fs::copy(src, file_path(sha).with_extension("in")).map_err(|e| e.to_string())?;
    std::fs::rename(file_path(sha).with_extension("in"), file_path(sha)).map_err(|e| e.to_string())?;
    st().have.insert(sha.to_string());
    Ok(())
}

/// Start: the trusted keys, the releases and files kept here, and the fetcher.
pub fn start(node: &Arc<Node>, test_key: Option<String>) {
    let mut keys: Vec<VerifyingKey> = RELEASE_KEYS.iter().filter_map(|k| hex::decode(k).ok()?.try_into().ok()).filter_map(|b: [u8; 32]| VerifyingKey::from_bytes(&b).ok()).collect();
    if let Some(k) = test_key.and_then(|k| hex::decode(k).ok()).and_then(|b| <[u8; 32]>::try_from(b).ok()).and_then(|b| VerifyingKey::from_bytes(&b).ok()) {
        keys.push(k);
    }
    let _ = KEYS.set(keys);
    let _ = HOME.set(node.home.clone());
    if let Ok(rd) = std::fs::read_dir(dir().join("releases")) {
        for e in rd.flatten() {
            if let Some(v) = std::fs::read_to_string(e.path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
                let _ = take_release(v["record"].as_str().unwrap_or(""), v["sig"].as_str().unwrap_or(""));
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(dir().join("files")) {
        let mut s = st();
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.len() == 64 {
                s.have.insert(n);
            }
        }
    }
    let node = node.clone();
    let _ = std::thread::Builder::new().name("arc code".into()).spawn(move || loop {
        fetch_round(&node);
        std::thread::sleep(Duration::from_secs(3));
    });
}

/// Fetch one missing file from a linked arcade that holds it (one file at a time, in 48 kB pieces).
fn fetch_round(_node: &Node) {
    let job = {
        let s = st();
        let missing: Vec<(String, u64)> = wanted(&s).into_iter().filter(|(sha, _)| !s.have.contains(sha) && !s.fetching.contains(sha)).collect();
        missing.into_iter().find_map(|(sha, bytes)| {
            let peer = s.peer_has.iter().find(|(p, has)| has.contains(&sha) && crate::crvia::linked(p)).map(|(p, _)| p.clone())?;
            Some((sha, bytes, peer))
        })
    };
    let Some((sha, bytes, peer)) = job else { return };
    st().fetching.insert(sha.clone());
    let t0 = Instant::now();
    let r = fetch(&peer, &sha, bytes);
    st().fetching.remove(&sha);
    match r {
        Ok(()) => eprintln!("arc code: fetched {} ({bytes} bytes) from {} in {:.1} s", &sha[..12], &peer[..12], t0.elapsed().as_secs_f64()),
        Err(e) => eprintln!("arc code: fetching {} from {}: {e}", &sha[..12], &peer[..12]),
    }
}

fn fetch(peer: &str, sha: &str, bytes: u64) -> Result<(), String> {
    use crate::crsource::Pipe;
    std::fs::create_dir_all(dir().join("files")).map_err(|e| e.to_string())?;
    let part = file_path(sha).with_extension("part");
    let mut out = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&part).map_err(|e| e.to_string())?;
    // Resume where an earlier try stopped.
    let mut at = out.metadata().map(|m| m.len()).unwrap_or(0).min(bytes);
    at -= at % CHUNK;
    out.set_len(at).map_err(|e| e.to_string())?;
    out.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
    // Up to WINDOW pieces asked at once, each written at its place as it comes; a gap left by an error ends the try
    // (the next one resumes from the last whole piece before it).
    let mut asked: std::collections::VecDeque<(u64, crate::crvia::Via)> = std::collections::VecDeque::new();
    let mut next = at;
    while at < bytes {
        while asked.len() < WINDOW && next < bytes {
            let mut via = crate::crvia::Via::open(peer)?;
            via.send(&Frame::new(json!({"t": "co", "op": "g", "id": via.id, "f": sha, "at": next})))?;
            asked.push_back((next, via));
            next += CHUNK;
        }
        let (want, mut via) = asked.pop_front().expect("asked above");
        let f = via.recv(Instant::now() + Duration::from_secs(30))?;
        if let Some(e) = f.head["err"].as_str() {
            return Err(format!("peer: {e}"));
        }
        let n = f.blob.len() as u64;
        if n == 0 || n > CHUNK || want + n > bytes || (n < CHUNK && want + n != bytes) {
            return Err("a wrong-sized piece".into());
        }
        out.seek(SeekFrom::Start(want)).map_err(|e| e.to_string())?;
        out.write_all(&f.blob).map_err(|e| e.to_string())?;
        at = want + n;
    }
    drop(out);
    let r = keep_file(sha, &part);
    let _ = std::fs::remove_file(&part);
    r
}

/// What this link hasn't been told: release records, and (when it changed) the files held here.
pub fn tick(peer: &str, link: &mut Link) -> Result<(), LinkError> {
    if KEYS.get().is_none() {
        return Ok(());
    }
    let (records, have) = {
        let mut s = st();
        let told = s.told.entry(peer.to_string()).or_default().clone();
        let new: Vec<(String, String, String)> = s.releases.values().filter(|r| !told.contains(&r.id)).map(|r| (r.id.clone(), r.record.clone(), r.sig.clone())).collect();
        let t = s.told.get_mut(peer).expect("made above");
        for (id, _, _) in &new {
            t.insert(id.clone());
        }
        let n = s.have.len();
        let have = (s.have_told.get(peer) != Some(&n)).then(|| s.have.iter().cloned().collect::<Vec<_>>());
        s.have_told.insert(peer.to_string(), n);
        (new, have)
    };
    for (_, record, sig) in records {
        link.send(&Frame::new(json!({"t": "co", "op": "r", "record": record, "sig": sig})))?;
    }
    if let Some(have) = have {
        link.send(&Frame::new(json!({"t": "co", "op": "h", "files": have})))?;
    }
    Ok(())
}

/// A "co" frame.
pub fn on_frame(link: &mut Link, f: &Frame) -> Result<(), LinkError> {
    if KEYS.get().is_none() {
        return Ok(());
    }
    match f.head["op"].as_str().unwrap_or("") {
        "r" => {
            if let (Some(record), Some(sig)) = (f.head["record"].as_str(), f.head["sig"].as_str()) {
                let _ = take_release(record, sig);
            }
        }
        "h" => {
            let files: HashSet<String> = f.head["files"].as_array().into_iter().flatten().filter_map(|x| x.as_str().filter(|s| s.len() == 64).map(String::from)).take(100_000).collect();
            st().peer_has.insert(link.peer_id.clone(), files);
        }
        "g" => {
            let id = f.head["id"].as_u64().unwrap_or(0);
            let (sha, at) = (f.head["f"].as_str().unwrap_or(""), f.head["at"].as_u64().unwrap_or(0));
            let ans = |head: Value, blob: Vec<u8>| Frame { head, blob };
            if sha.len() != 64 || !st().have.contains(sha) {
                return link.send(&ans(json!({"t": "co", "op": "a", "id": id, "err": "not held here"}), Vec::new()));
            }
            let mut blob = vec![0u8; CHUNK as usize];
            let n = std::fs::File::open(file_path(sha)).and_then(|mut fh| {
                fh.seek(SeekFrom::Start(at))?;
                let mut got = 0;
                while got < blob.len() {
                    let k = fh.read(&mut blob[got..])?;
                    if k == 0 {
                        break;
                    }
                    got += k;
                }
                Ok(got)
            });
            match n {
                Ok(n) => {
                    blob.truncate(n);
                    link.send(&ans(json!({"t": "co", "op": "a", "id": id}), blob))?;
                }
                Err(e) => link.send(&ans(json!({"t": "co", "op": "a", "id": id, "err": e.to_string()}), Vec::new()))?,
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------- the local API ----------------

/// `code.status`: the releases known here and which of their files are held.
pub fn status() -> Value {
    let s = st();
    let want = wanted(&s);
    let releases: Vec<Value> = s
        .releases
        .values()
        .map(|r| {
            let files: Vec<Value> = r.files.iter().map(|(n, (sha, b))| json!({"name": n, "sha256": sha, "bytes": b, "held": s.have.contains(sha), "kept": want.contains_key(sha)})).collect();
            json!({"id": r.id, "name": r.name, "version": r.version, "commit": r.commit, "time": r.time, "files": files})
        })
        .collect();
    let missing: u64 = want.iter().filter(|(sha, _)| !s.have.contains(*sha)).map(|(_, b)| b).sum();
    json!({"trusted_keys": KEYS.get().map(|k| k.iter().map(|k| hex::encode(k.to_bytes())).collect::<Vec<_>>()).unwrap_or_default(),
           "releases": releases, "files_held": s.have.len(), "bytes_missing": missing, "fetching": s.fetching.iter().collect::<Vec<_>>()})
}

/// `code.add {record, sig, files: {name: path}}`: a release made here (scripts/code-publish.py), with its files.
pub fn add(req: &Value) -> Result<Value, String> {
    let (record, sig) = (req["record"].as_str().ok_or("record required")?, req["sig"].as_str().ok_or("sig required")?);
    let r = Release::check(record, sig)?;
    for (name, (sha, _)) in &r.files {
        if st().have.contains(sha) {
            continue;
        }
        let path = req["files"][name].as_str().ok_or(format!("the file {name} is needed"))?;
        keep_file(sha, Path::new(path))?;
    }
    let new = take_release(record, sig)?;
    Ok(json!({"id": r.id, "new": new}))
}

/// `code.export {id, dir}`: a release's held files written into `dir` under their names.
pub fn export(req: &Value) -> Result<Value, String> {
    let id = req["id"].as_str().ok_or("id required")?;
    let to = PathBuf::from(req["dir"].as_str().ok_or("dir required")?);
    let r = st().releases.get(id).cloned().ok_or("no such release here")?;
    std::fs::create_dir_all(&to).map_err(|e| e.to_string())?;
    let mut written = Vec::new();
    for (name, (sha, _)) in &r.files {
        if file_path(sha).exists() {
            std::fs::copy(file_path(sha), to.join(name)).map_err(|e| e.to_string())?;
            written.push(name.clone());
        }
    }
    std::fs::write(to.join("release.json"), json!({"record": r.record, "sig": r.sig}).to_string()).map_err(|e| e.to_string())?;
    Ok(json!({"id": r.id, "dir": to, "written": written, "commit": r.commit}))
}
