//! Git repositories from ARCADE blobs (docs/ARCADECHAIN.md §15 step 6).
//!
//! A repository's state is a **manifest blob** (JSON): `{"git": "manifest", "name", "head": "refs/heads/main",
//! "refs": {"refs/heads/main": "<commit sha>", ...}, "packs": [{"name": "<pack sha1>", "pack": "<blob id>", "idx":
//! "<blob id>"}], "prev": "<the manifest before>"}`. Each push adds packs of only the objects that are new since the
//! manifest before (git `pack-objects`, split under one block's 4 MB) and their indexes, all blobs, and a new manifest
//! listing every pack so far. scripts/git-publish.py makes them.
//!
//! The arcade answers git's **dumb HTTP protocol** for a manifest, which every git client speaks without anything
//! running on the arcade but files: `git clone http://<arcade>/git/<manifest id>` (no trailing parts). Packs and
//! indexes are fetched like any blob a viewer asks for (crate::arcblob::view), so a repository many clone spreads.

use serde_json::Value;

/// A blob the ARCADE chain holds (only what a block brought is a repository's part), for a viewer.
fn on_chain(id: &str) -> Option<Vec<u8>> {
    let bid: [u8; 32] = hex::decode(id).ok()?.try_into().ok()?;
    crate::arcsvc::holds_blob(&bid).then(|| crate::arcblob::view(&bid)).flatten()
}

fn manifest(id: &str) -> Option<Value> {
    let v: Value = serde_json::from_slice(&on_chain(id)?).ok()?;
    (v["git"] == "manifest").then_some(v)
}

fn sha1ish(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// `rest` = `<manifest id>/<git path>`: the status line and body.
pub fn serve(rest: &str) -> (&'static str, Vec<u8>) {
    let (id, file) = rest.split_once('/').unwrap_or((rest, ""));
    let Some(m) = manifest(id.trim_end_matches(".git")) else { return ("404 Not Found", b"no such repository manifest on the ARCADE chain\n".to_vec()) };
    let refs = m["refs"].as_object().cloned().unwrap_or_default();
    match file {
        "HEAD" => {
            let head = m["head"].as_str().unwrap_or("refs/heads/main");
            ("200 OK", format!("ref: {head}\n").into_bytes())
        }
        "info/refs" => {
            let mut lines: Vec<String> = refs.iter().filter_map(|(name, sha)| sha.as_str().filter(|s| sha1ish(s)).map(|s| format!("{s}\t{name}\n"))).collect();
            lines.sort();
            ("200 OK", lines.concat().into_bytes())
        }
        "objects/info/packs" => {
            let mut out = String::new();
            for p in m["packs"].as_array().into_iter().flatten() {
                if let Some(n) = p["name"].as_str().filter(|n| sha1ish(n)) {
                    out.push_str(&format!("P pack-{n}.pack\n"));
                }
            }
            out.push('\n');
            ("200 OK", out.into_bytes())
        }
        f if f.starts_with("objects/pack/pack-") => {
            let leaf = &f["objects/pack/pack-".len()..];
            let (name, ext) = leaf.rsplit_once('.').unwrap_or((leaf, ""));
            let found = m["packs"].as_array().into_iter().flatten().find(|p| p["name"] == name);
            let key = match ext {
                "pack" => "pack",
                "idx" => "idx",
                _ => return ("404 Not Found", Vec::new()),
            };
            match found.and_then(|p| p[key].as_str()).and_then(on_chain) {
                Some(b) => ("200 OK", b),
                None => ("404 Not Found", b"that pack isn't here nor at the arcades linked to this one\n".to_vec()),
            }
        }
        // Loose objects, alternates: none (a client then uses the packs).
        _ => ("404 Not Found", Vec::new()),
    }
}
