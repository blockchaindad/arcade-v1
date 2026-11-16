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
    // Arcade V1.0's releases as a repository (crate::relrepo): made-up ids for its manifest, pack and index.
    if id.starts_with("release-") {
        return crate::relrepo::blob(id);
    }
    let bid: [u8; 32] = hex::decode(id).ok()?.try_into().ok()?;
    crate::arcsvc::holds_blob(&bid).then(|| whole(&bid)).flatten()
}

/// The most a file of many blobs read here as one git file may be (a pack bigger than a blob: its parts in a file
/// manifest, crate::arcblob::file_manifest, as scripts/git-remote-arcade publishes one holding a file over 4 MB).
const MAX_JOINED: u64 = 256 << 20;

/// A blob's bytes; a file manifest's whole file, its parts joined.
fn whole(id: &[u8; 32]) -> Option<Vec<u8>> {
    if let Some(f) = crate::arcblob::file_manifest(id).filter(|f| f.size <= MAX_JOINED) {
        let mut out = Vec::with_capacity(f.size as usize);
        return crate::arcblob::file_range(&f, 0, f.size.saturating_sub(1), |b| { out.extend_from_slice(b); true }).then_some(out);
    }
    crate::arcblob::view(id)
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
        if let Some(m) = crate::relrepo::manifest_of(node, &crate::relrepo::plain(&format!("{owner}/{name}"))) {
            return serve_manifest(&m, file);
        }
        let Ok(full) = arcade_ledger::repos::full_name(&format!("{owner}/{name}"), None) else { return ("404 Not Found", b"not a repository name\n".to_vec()) };
        let Some(m) = crate::arcledger::repo_manifest(node, &full).or_else(|| crate::relrepo::manifest_of(node, &full)) else { return ("404 Not Found", format!("no repository @{full} on the ARCADE chain\n").into_bytes()) };
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

/// A git read here, by its path: `/r/git/...` (page_read) or `/git/...` (serve, the dumb HTTP files). None for any other.
pub fn here(node: &crate::node::Node, path: &str, q: &serde_json::Map<String, Value>) -> Option<PageAnswer> {
    if let Some(rest) = path.strip_prefix("/git/") {
        let (status, body) = serve(node, rest);
        let code = status.split(' ').next().and_then(|c| c.parse().ok()).unwrap_or(500);
        return Some(PageAnswer { code, ctype: if code == 200 { "application/octet-stream" } else { "text/plain" }, gzip: false, body });
    }
    page_read(node, path, q)
}

/// A git read on an arcade that only borrows the ARCADE chain (crate::arcborrow): it holds no ledger and no blobs, so
/// the read is asked of an arcade lending the chain (rpc.rs "arcade_git"), which answers from its own copy. That
/// arcade's word, as a borrowed chain's blocks are before their headers check them; git checks every object it
/// clones by its hash. Packs, indexes and blobs never change: kept here a while (BORROWED_CACHE bytes in all).
/// None while the chain runs here (or isn't defined), and for paths that aren't git reads.
pub fn lent(node: &crate::node::Node, path: &str, q: &serde_json::Map<String, Value>) -> Option<PageAnswer> {
    let p = crate::arcsvc::defined()?;
    if crate::arcsvc::runs(p) || !(path.starts_with("/r/git/") || path.starts_with("/git/")) {
        return None;
    }
    let fixed = path.starts_with("/r/git/blob/") || path.contains("/objects/pack/pack-");
    type Cache = std::collections::VecDeque<(String, PageAnswer)>;
    static C: std::sync::Mutex<Cache> = std::sync::Mutex::new(std::collections::VecDeque::new());
    if fixed {
        if let Some((_, a)) = C.lock().unwrap().iter().find(|(k, _)| k == path) {
            return Some(PageAnswer { code: a.code, ctype: a.ctype, gzip: a.gzip, body: a.body.clone() });
        }
    }
    let a = match crate::borrow::ask_lender(node, p, "arcade_git", serde_json::json!([path, q])) {
        Ok(v) => {
            use base64::Engine;
            let ctype = match v["ctype"].as_str().unwrap_or("") {
                "application/json" => "application/json",
                "text/plain" => "text/plain",
                _ => "application/octet-stream",
            };
            let body = base64::engine::general_purpose::STANDARD.decode(v["b64"].as_str().unwrap_or("")).unwrap_or_default();
            PageAnswer { code: v["code"].as_u64().unwrap_or(502) as u16, ctype, gzip: v["gzip"] == true, body }
        }
        Err(e) => {
            let body = if path.starts_with("/r/git/") { serde_json::json!({"ok": false, "error": format!("this arcade borrows the ARCADE chain; asking an arcade that lends it: {e}")}).to_string() } else { format!("this arcade borrows the ARCADE chain; asking an arcade that lends it: {e}\n") };
            return Some(PageAnswer { code: 502, ctype: if path.starts_with("/r/git/") { "application/json" } else { "text/plain" }, gzip: false, body: body.into_bytes() });
        }
    };
    if fixed && a.code == 200 {
        let mut c = C.lock().unwrap();
        c.push_back((path.to_string(), PageAnswer { code: a.code, ctype: a.ctype, gzip: a.gzip, body: a.body.clone() }));
        while c.iter().map(|(_, x)| x.body.len()).sum::<usize>() > BORROWED_CACHE {
            c.pop_front();
        }
    }
    Some(a)
}

/// Bytes of packs, indexes and blobs an arcade borrowing the ARCADE chain keeps from its lender (crate::arcgit::lent).
const BORROWED_CACHE: usize = 96 << 20;

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
        "repos" => json_answer(crate::arcledger::repos(node, s("owner").as_deref()).map(|mut v| {
            // Arcade V1.0's own code, from its releases (crate::relrepo), unless the ledger has that repository.
            let owner = s("owner").map(|o| o.trim_start_matches('@').to_lowercase());
            if owner.as_deref().is_none_or(|o| o == "arcade") && crate::arcledger::repo_row(node, crate::relrepo::FULL).is_none() {
                if let (Some(row), Some(list)) = (crate::relrepo::row(), v["repos"].as_array_mut()) {
                    list.push(row);
                }
            }
            v
        })),
        "repo" => match s("name") {
            Some(n) if crate::relrepo::is_it(node, &crate::relrepo::plain(&n)) => {
                json_answer(crate::relrepo::info().ok_or_else(|| "no release of Arcade V1.0 is held here".to_string()))
            }
            Some(n) => json_answer(crate::arcledger::repo(node, &n)),
            None => json_answer(Err("name required (@owner/name)".into())),
        },
        "items" => {
            let mut req = serde_json::json!({"repo": s("repo")});
            if let Some(n) = s("number").and_then(|n| n.parse::<u64>().ok()) {
                req["number"] = serde_json::json!(n);
            }
            if s("repo").is_some_and(|r| crate::relrepo::is_it(node, &crate::relrepo::plain(&r))) {
                // The releases' repository: no ledger records (yet), so no issues or pull requests.
                json_answer(Ok(serde_json::json!({"repo": format!("@{}", crate::relrepo::FULL), "items": []})))
            } else if req["repo"].is_null() {
                json_answer(Err("repo required (@owner/name)".into()))
            } else {
                json_answer(crate::arcledger::items(node, &req))
            }
        }
        r if r.starts_with("blob/") => {
            let id = &r["blob/".len()..];
            if id.starts_with("release-") {
                return Some(match crate::relrepo::blob(id) {
                    Some(body) => PageAnswer { code: 200, ctype: "application/octet-stream", gzip: false, body },
                    None => PageAnswer { code: 404, ctype: "text/plain", gzip: false, body: b"no such release file here\n".to_vec() },
                });
            }
            let bid: Option<[u8; 32]> = hex::decode(id).ok().and_then(|b| b.try_into().ok());
            match bid.filter(crate::arcsvc::holds_blob).and_then(|b| whole(&b).map(|bytes| (b, bytes))) {
                Some((b, body)) => PageAnswer { code: 200, ctype: "application/octet-stream", gzip: crate::arcblob::gzipped(&b) && crate::arcblob::file_manifest(&b).is_none(), body },
                None => PageAnswer { code: 404, ctype: "text/plain", gzip: false, body: b"no such blob on the ARCADE chain here nor at the arcades linked to this one\n".to_vec() },
            }
        }
        _ => PageAnswer { code: 404, ctype: "application/json", gzip: false, body: br#"{"ok":false,"error":"no such git read"}"#.to_vec() },
    })
}
