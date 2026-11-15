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

/// `rest` = `<manifest id>/<git path>`, or `@owner/name/<git path>` for a repository under an @tag (§15 step 9: its
/// manifest now, as the ARCADE chain's ledger says): the status line and body.
pub fn serve(node: &crate::node::Node, rest: &str) -> (&'static str, Vec<u8>) {
    if let Some(named) = rest.strip_prefix('@').or_else(|| rest.strip_prefix("%40")) {
        let mut parts = named.splitn(3, '/');
        let (owner, name, file) = (parts.next().unwrap_or(""), parts.next().unwrap_or("").trim_end_matches(".git"), parts.next().unwrap_or(""));
        let Ok(full) = arcade_ledger::repos::full_name(&format!("{owner}/{name}"), None) else { return ("404 Not Found", b"not a repository name\n".to_vec()) };
        let Some(m) = crate::arcledger::repo_manifest(node, &full) else { return ("404 Not Found", format!("no repository @{full} on the ARCADE chain\n").into_bytes()) };
        return serve_manifest(&m, file);
    }
    let (id, file) = rest.split_once('/').unwrap_or((rest, ""));
    serve_manifest(id, file)
}

fn serve_manifest(id: &str, file: &str) -> (&'static str, Vec<u8>) {
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

/// One answer of the git page reads: (status code, content type, whether the body is gzipped, body).
pub struct PageAnswer {
    pub code: u16,
    pub ctype: &'static str,
    pub gzip: bool,
    pub body: Vec<u8>,
}

fn json_answer(r: Result<Value, String>) -> PageAnswer {
    let (code, v) = match r {
        Ok(v) => (200, serde_json::json!({"ok": true, "result": v})),
        Err(e) => (if e.contains("no repository") { 404 } else { 400 }, serde_json::json!({"ok": false, "error": e})),
    };
    PageAnswer { code, ctype: "application/json", gzip: false, body: v.to_string().into_bytes() }
}

/// The repositories of the ARCADE chain for apps (GET, public data, no page pass needed), so an app can show them as a
/// code host does:
/// - `/r/git/repos?owner=<tag>`: every repository (ledger.repos);
/// - `/r/git/repo?name=@owner/name`: one, with its maintainers and push history (ledger.repo);
/// - `/r/git/items?repo=@owner/name&number=<n>`: its issues and pull requests with their texts (ledger.items);
/// - `/r/git/blob/<id>`: a blob the ARCADE chain holds, as stored (a manifest, a pack, an index, a text), which the app
///   reads git objects from itself.
///
/// None for any other path.
pub fn page_read(node: &crate::node::Node, path: &str, q: &serde_json::Map<String, Value>) -> Option<PageAnswer> {
    let rest = path.strip_prefix("/r/git/")?;
    let s = |k: &str| q.get(k).and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }).filter(|s| !s.is_empty());
    Some(match rest {
        "repos" => json_answer(crate::arcledger::repos(node, s("owner").as_deref())),
        "repo" => match s("name") {
            Some(n) => json_answer(crate::arcledger::repo(node, &n)),
            None => json_answer(Err("name required (@owner/name)".into())),
        },
        "items" => {
            let mut req = serde_json::json!({"repo": s("repo")});
            if let Some(n) = s("number").and_then(|n| n.parse::<u64>().ok()) {
                req["number"] = serde_json::json!(n);
            }
            if req["repo"].is_null() {
                json_answer(Err("repo required (@owner/name)".into()))
            } else {
                json_answer(crate::arcledger::items(node, &req))
            }
        }
        r if r.starts_with("blob/") => {
            let id = &r["blob/".len()..];
            let bid: Option<[u8; 32]> = hex::decode(id).ok().and_then(|b| b.try_into().ok());
            match bid.filter(crate::arcsvc::holds_blob).and_then(|b| crate::arcblob::view(&b).map(|bytes| (b, bytes))) {
                Some((b, body)) => PageAnswer { code: 200, ctype: "application/octet-stream", gzip: crate::arcblob::gzipped(&b), body },
                None => PageAnswer { code: 404, ctype: "text/plain", gzip: false, body: b"no such blob on the ARCADE chain here nor at the arcades linked to this one\n".to_vec() },
            }
        }
        _ => PageAnswer { code: 404, ctype: "application/json", gzip: false, body: br#"{"ok":false,"error":"no such git read"}"#.to_vec() },
    })
}
