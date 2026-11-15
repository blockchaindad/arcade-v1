//! Arcade V1.0's own code as a repository (Martin 2026-11-15: show the releases in Arcade Git rather than push the
//! code a second time). Every arcade keeps every `@arcade/arcade-v1` release's git bundle (crate::arccode); a bundle
//! is a header naming refs and then a git pack, so the newest held one is served read-only as the repository
//! `@arcade/arcade-v1` (unless the ARCADE chain's ledger has a repository of that name, which then wins):
//!
//! - its manifest is made here: `release-<release id>`, with `refs/heads/main` at the release's commit and a tag per
//!   release whose commit the pack holds;
//! - its pack is the bundle's pack (`release-<bundle sha256>`), and its index (`release-<bundle sha256>.idx`) is built
//!   here once (as `git index-pack` does) and kept beside the bundle.
//!
//! Through `/r/git/...` (apps) and `/git/@arcade/arcade-v1` (`git clone`), as any repository.

use flate2::{Decompress, FlushDecompress, Status};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const FULL: &str = "arcade/arcade-v1";
const BUNDLE: &str = "arcade-v1.bundle";

pub struct RelRepo {
    pub release: crate::arccode::Release,
    pub bundle: PathBuf,
    pub bundle_sha: String,
}

/// The newest release whose bundle is held here.
pub fn newest() -> Option<RelRepo> {
    let (release, bundle) = crate::arccode::held_files(crate::arccode::ARCADE_V1, BUNDLE).into_iter().next()?;
    let bundle_sha = release.files.get(BUNDLE)?.0.clone();
    Some(RelRepo { release, bundle, bundle_sha })
}

/// A name as `owner/name`, lowercase (`@arcade/arcade-v1.git` -> `arcade/arcade-v1`): @arcade is a reserved tag, so
/// the ledger's name check refuses it.
pub fn plain(name: &str) -> String {
    name.trim().trim_start_matches('@').trim_end_matches(".git").to_lowercase()
}

/// Whether `full` (`owner/name`) is the releases' repository and the ledger has none of that name.
pub fn is_it(node: &crate::node::Node, full: &str) -> bool {
    full == FULL && crate::arcledger::repo_row(node, FULL).is_none() && newest().is_some()
}

/// The bundle's refs and where its pack starts.
fn split(bundle: &[u8]) -> Result<(Vec<(String, String)>, usize), String> {
    let mut refs = Vec::new();
    let mut at = 0;
    let mut first = true;
    loop {
        let end = bundle[at..].iter().position(|b| *b == b'\n').ok_or("not a git bundle")? + at;
        let line = std::str::from_utf8(&bundle[at..end]).map_err(|_| "not a git bundle")?;
        at = end + 1;
        if first {
            if line != "# v2 git bundle" && line != "# v3 git bundle" {
                return Err("not a git bundle".into());
            }
            first = false;
            continue;
        }
        if line.is_empty() {
            break;
        }
        if line.starts_with('@') || line.starts_with('-') {
            continue;
        }
        if let Some((sha, name)) = line.split_once(' ') {
            refs.push((sha.to_string(), name.to_string()));
        }
    }
    if bundle.get(at..at + 4) != Some(b"PACK") {
        return Err("the bundle holds no pack".into());
    }
    Ok((refs, at))
}

static IDX: Mutex<Option<(String, Arc<Vec<u8>>, Arc<Vec<String>>)>> = Mutex::new(None);

/// The bundle's pack and its index (built once, kept beside the bundle), with the objects it holds.
fn pack_and_index(r: &RelRepo) -> Result<(Vec<u8>, Arc<Vec<u8>>, Arc<Vec<String>>), String> {
    let bundle = std::fs::read(&r.bundle).map_err(|e| e.to_string())?;
    let (_, at) = split(&bundle)?;
    let pack = bundle[at..].to_vec();
    let mut g = IDX.lock().unwrap();
    if let Some((sha, idx, objs)) = g.as_ref() {
        if *sha == r.bundle_sha {
            return Ok((pack, idx.clone(), objs.clone()));
        }
    }
    let kept = r.bundle.with_extension("idx");
    let idx = match std::fs::read(&kept) {
        Ok(b) if b.len() > 1072 => b,
        _ => {
            let b = index_pack(&pack)?;
            let _ = std::fs::write(&kept, &b);
            b
        }
    };
    let n = u32::from_be_bytes(idx[8 + 255 * 4..8 + 256 * 4].try_into().unwrap()) as usize;
    let objs: Vec<String> = (0..n).map(|i| hex::encode(&idx[1032 + i * 20..1032 + i * 20 + 20])).collect();
    let (idx, objs) = (Arc::new(idx), Arc::new(objs));
    *g = Some((r.bundle_sha.clone(), idx.clone(), objs.clone()));
    Ok((pack, idx, objs))
}

fn manifest_id(r: &RelRepo) -> String {
    format!("release-{}", r.release.id)
}

fn manifest(r: &RelRepo) -> Result<Value, String> {
    let bundle = std::fs::read(&r.bundle).map_err(|e| e.to_string())?;
    let (brefs, at) = split(&bundle)?;
    let (_, _, objs) = pack_and_index(r)?;
    let name = hex::encode(&bundle[bundle.len() - 20..]);
    let _ = at;
    let mut refs = serde_json::Map::new();
    for (sha, n) in brefs {
        if n.starts_with("refs/") {
            refs.insert(n, json!(sha));
        }
    }
    refs.insert("refs/heads/main".into(), json!(r.release.commit));
    for rel in crate::arccode::releases_of(crate::arccode::ARCADE_V1) {
        if objs.binary_search(&rel.commit).is_ok() {
            refs.insert(format!("refs/tags/{}", rel.version), json!(rel.commit));
        }
    }
    Ok(json!({"git": "manifest", "name": FULL, "head": "refs/heads/main", "refs": refs,
              "packs": [{"name": name, "pack": format!("release-{}", r.bundle_sha), "idx": format!("release-{}.idx", r.bundle_sha)}],
              "prev": null, "time": r.release.time, "release": {"id": r.release.id, "version": r.release.version, "commit": r.release.commit}}))
}

/// A blob of the releases' repository by its made-up id (`release-...`): the manifest, the pack or its index.
pub fn blob(id: &str) -> Option<Vec<u8>> {
    let rest = id.strip_prefix("release-")?;
    let r = newest()?;
    if rest == r.release.id {
        return serde_json::to_vec(&manifest(&r).ok()?).ok();
    }
    if rest == r.bundle_sha {
        return pack_and_index(&r).ok().map(|x| x.0);
    }
    if rest.strip_suffix(".idx") == Some(&r.bundle_sha) {
        return pack_and_index(&r).ok().map(|x| x.1.to_vec());
    }
    None
}

/// `ledger.repo`'s answer for the releases' repository.
pub fn info() -> Option<Value> {
    let r = newest()?;
    let log: Vec<Value> = crate::arccode::releases_of(crate::arccode::ARCADE_V1).iter().rev()
        .map(|x| json!({"m": format!("release-{}", x.id), "by": "arcade", "txid": null, "height": null, "version": x.version, "time": x.time}))
        .collect();
    Some(json!({"repo": format!("@{FULL}"), "manifest": manifest_id(&r), "fork": null, "pushes": log.len(), "by": "arcade",
                "height": null, "maintainers": [], "log": log,
                "release": {"id": r.release.id, "version": r.release.version, "commit": r.release.commit, "time": r.release.time}}))
}

/// The row `ledger.repos` lists for it.
pub fn row() -> Option<Value> {
    let r = newest()?;
    Some(json!({"repo": format!("@{FULL}"), "manifest": manifest_id(&r), "fork": null,
                "pushes": crate::arccode::releases_of(crate::arccode::ARCADE_V1).len(), "release": r.release.version}))
}

/// The manifest id `/git/@arcade/arcade-v1` serves.
pub fn manifest_of(node: &crate::node::Node, full: &str) -> Option<String> {
    is_it(node, full).then(|| newest().map(|r| manifest_id(&r))).flatten()
}

// ---------------- index-pack ----------------

const TYPE_NAMES: [&str; 5] = ["", "commit", "tree", "blob", "tag"];

enum Base {
    None,
    Ofs(usize),
    Ref([u8; 20]),
}

struct Entry {
    off: usize,
    kind: u8,
    base: Base,
    data: usize,
    end: usize,
    crc: u32,
}

fn inflate(b: &[u8], size: usize) -> Result<(Vec<u8>, usize), String> {
    let mut d = Decompress::new(true);
    let mut out = Vec::with_capacity(size.max(16));
    loop {
        if out.len() == out.capacity() {
            out.reserve(4096.max(out.capacity()));
        }
        let st = d.decompress_vec(&b[d.total_in() as usize..], &mut out, FlushDecompress::None).map_err(|e| format!("a pack object doesn't inflate: {e}"))?;
        if st == Status::StreamEnd {
            return Ok((out, d.total_in() as usize));
        }
        if d.total_in() as usize >= b.len() && out.len() < out.capacity() {
            return Err("a pack object ends early".into());
        }
    }
}

fn apply_delta(src: &[u8], d: &[u8]) -> Result<Vec<u8>, String> {
    let mut p = 0;
    let mut varint = || -> Result<usize, String> {
        let (mut r, mut s) = (0usize, 0);
        loop {
            let c = *d.get(p).ok_or("a delta ends early")?;
            p += 1;
            r |= ((c & 0x7f) as usize) << s;
            s += 7;
            if c & 0x80 == 0 {
                return Ok(r);
            }
        }
    };
    let src_len = varint()?;
    let size = varint()?;
    if src_len != src.len() {
        return Err("a delta names another base".into());
    }
    let mut out = Vec::with_capacity(size);
    while p < d.len() {
        let c = d[p];
        p += 1;
        if c & 0x80 != 0 {
            let (mut off, mut n) = (0usize, 0usize);
            for i in 0..4 {
                if c & (1 << i) != 0 {
                    off |= (*d.get(p).ok_or("a delta ends early")? as usize) << (8 * i);
                    p += 1;
                }
            }
            for i in 0..3 {
                if c & (16 << i) != 0 {
                    n |= (*d.get(p).ok_or("a delta ends early")? as usize) << (8 * i);
                    p += 1;
                }
            }
            if n == 0 {
                n = 0x10000;
            }
            out.extend_from_slice(src.get(off..off + n).ok_or("a delta copies past its base")?);
        } else if c != 0 {
            out.extend_from_slice(d.get(p..p + c as usize).ok_or("a delta ends early")?);
            p += c as usize;
        } else {
            return Err("a delta has a zero op".into());
        }
    }
    if out.len() != size {
        return Err("a delta makes the wrong size".into());
    }
    Ok(out)
}

/// A version 2 index of a whole (not thin) pack, as `git index-pack` writes it.
pub fn index_pack(pack: &[u8]) -> Result<Vec<u8>, String> {
    if pack.len() < 32 || &pack[..4] != b"PACK" {
        return Err("not a pack".into());
    }
    let n = u32::from_be_bytes(pack[8..12].try_into().unwrap()) as usize;
    let body_end = pack.len() - 20;
    let mut es: Vec<Entry> = Vec::with_capacity(n);
    let mut pos = 12;
    for _ in 0..n {
        let off = pos;
        let mut c = *pack.get(pos).ok_or("the pack ends early")?;
        pos += 1;
        let kind = (c >> 4) & 7;
        let (mut size, mut shift) = ((c & 15) as usize, 4);
        while c & 0x80 != 0 {
            c = *pack.get(pos).ok_or("the pack ends early")?;
            pos += 1;
            size |= ((c & 0x7f) as usize) << shift;
            shift += 7;
        }
        let base = match kind {
            6 => {
                c = *pack.get(pos).ok_or("the pack ends early")?;
                pos += 1;
                let mut o = (c & 0x7f) as usize;
                while c & 0x80 != 0 {
                    c = *pack.get(pos).ok_or("the pack ends early")?;
                    pos += 1;
                    o = ((o + 1) << 7) | (c & 0x7f) as usize;
                }
                Base::Ofs(off.checked_sub(o).ok_or("a delta's base is before the pack")?)
            }
            7 => {
                let b: [u8; 20] = pack.get(pos..pos + 20).ok_or("the pack ends early")?.try_into().unwrap();
                pos += 20;
                Base::Ref(b)
            }
            1..=4 => Base::None,
            _ => return Err(format!("unknown pack object type {kind}")),
        };
        let (_, used) = inflate(&pack[pos..body_end], size)?;
        let data = pos;
        pos += used;
        es.push(Entry { off, kind, base, data, end: pos, crc: crc32(&pack[off..pos]) });
    }
    if pos != body_end {
        return Err("the pack has bytes after its objects".into());
    }
    let at: HashMap<usize, usize> = es.iter().enumerate().map(|(i, e)| (e.off, i)).collect();
    let mut shas: Vec<Option<[u8; 20]>> = vec![None; n];
    let mut by_sha: HashMap<[u8; 20], usize> = HashMap::new();
    let mut cache: HashMap<usize, (u8, Arc<Vec<u8>>)> = HashMap::new();
    let mut cached = 0usize;
    // Objects whose base (by sha) isn't known yet wait for a later pass.
    let mut left: Vec<usize> = (0..n).collect();
    while !left.is_empty() {
        let before = left.len();
        let mut next = Vec::new();
        for i in left {
            match resolve(pack, &es, &at, &by_sha, &mut cache, &mut cached, i) {
                Ok(Some((kind, data))) => {
                    let mut h = Sha1::new();
                    h.update(format!("{} {}\0", TYPE_NAMES[kind as usize], data.len()).as_bytes());
                    h.update(&data[..]);
                    let sha: [u8; 20] = h.finalize().into();
                    shas[i] = Some(sha);
                    by_sha.insert(sha, i);
                }
                Ok(None) => next.push(i),
                Err(e) => return Err(e),
            }
        }
        if next.len() == before {
            return Err("a delta's base isn't in the pack (a thin pack)".into());
        }
        left = next;
    }
    let mut rows: Vec<([u8; 20], u32, usize)> = es.iter().enumerate().map(|(i, e)| (shas[i].unwrap(), e.crc, e.off)).collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut idx = Vec::with_capacity(1072 + n * 28);
    idx.extend_from_slice(&[0xff, 0x74, 0x4f, 0x63, 0, 0, 0, 2]);
    let mut j = 0;
    for b in 0..256usize {
        while j < n && (rows[j].0[0] as usize) <= b {
            j += 1;
        }
        idx.extend_from_slice(&(j as u32).to_be_bytes());
    }
    for r in &rows {
        idx.extend_from_slice(&r.0);
    }
    for r in &rows {
        idx.extend_from_slice(&r.1.to_be_bytes());
    }
    let mut large = Vec::new();
    for r in &rows {
        if r.2 < 0x8000_0000 {
            idx.extend_from_slice(&(r.2 as u32).to_be_bytes());
        } else {
            idx.extend_from_slice(&(0x8000_0000u32 | large.len() as u32).to_be_bytes());
            large.push(r.2 as u64);
        }
    }
    for l in large {
        idx.extend_from_slice(&l.to_be_bytes());
    }
    idx.extend_from_slice(&pack[body_end..]);
    let sum: [u8; 20] = Sha1::digest(&idx).into();
    idx.extend_from_slice(&sum);
    Ok(idx)
}

/// An object's type and bytes; None when it is a delta on an object (by sha) not resolved yet.
fn resolve(pack: &[u8], es: &[Entry], at: &HashMap<usize, usize>, by_sha: &HashMap<[u8; 20], usize>,
           cache: &mut HashMap<usize, (u8, Arc<Vec<u8>>)>, cached: &mut usize, i: usize) -> Result<Option<(u8, Arc<Vec<u8>>)>, String> {
    if let Some(x) = cache.get(&i) {
        return Ok(Some(x.clone()));
    }
    let e = &es[i];
    let (raw, _) = inflate(&pack[e.data..e.end], 0)?;
    let out = match e.base {
        Base::None => (e.kind, Arc::new(raw)),
        Base::Ofs(b) => {
            let bi = *at.get(&b).ok_or("a delta's base isn't an object of the pack")?;
            let Some((k, src)) = resolve(pack, es, at, by_sha, cache, cached, bi)? else { return Ok(None) };
            (k, Arc::new(apply_delta(&src, &raw)?))
        }
        Base::Ref(s) => {
            let Some(&bi) = by_sha.get(&s) else { return Ok(None) };
            let Some((k, src)) = resolve(pack, es, at, by_sha, cache, cached, bi)? else { return Ok(None) };
            (k, Arc::new(apply_delta(&src, &raw)?))
        }
    };
    // Bases are mostly near what uses them: keep up to 256 MB, then start over.
    if *cached > 256 << 20 {
        cache.clear();
        *cached = 0;
    }
    *cached += out.1.len();
    cache.insert(i, out.clone());
    Ok(Some(out))
}

fn crc32(b: &[u8]) -> u32 {
    static T: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, x) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *x = c;
        }
        t
    });
    let mut c = 0xffff_ffffu32;
    for &x in b {
        c = t[((c ^ x as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pack git made (with deltas) gets the very index `git index-pack` writes.
    #[test]
    fn index_matches_git() {
        let dir = std::env::temp_dir().join(format!("relrepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |a: &[&str]| assert!(std::process::Command::new("git").current_dir(&dir).args(a).output().unwrap().status.success(), "{a:?}");
        git(&["init", "-q"]);
        let mut text = String::new();
        for i in 0..40 {
            for j in 0..200 {
                text.push_str(&format!("line {j} of a file that changes a little: {}\n", if j == i { "changed" } else { "same" }));
            }
            std::fs::write(dir.join("f.txt"), &text).unwrap();
            git(&["add", "."]);
            git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", &format!("c{i}")]);
        }
        git(&["bundle", "create", "b.bundle", "--all"]);
        let bundle = std::fs::read(dir.join("b.bundle")).unwrap();
        let (refs, at) = split(&bundle).unwrap();
        assert!(!refs.is_empty());
        let pack = &bundle[at..];
        std::fs::write(dir.join("p.pack"), pack).unwrap();
        git(&["index-pack", "-o", "p.idx", "p.pack"]);
        let ours = index_pack(pack).unwrap();
        assert_eq!(ours, std::fs::read(dir.join("p.idx")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
