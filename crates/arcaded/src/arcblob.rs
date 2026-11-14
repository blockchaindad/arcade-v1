//! Blobs on the arcade's side (docs/ARCADECHAIN.md §15 step 1; the rules are arcade-arc `blob.rs`).
//!
//! - **The blob store**, `<home>/arc/<net>/blobs/<id>`: the bytes of every blob a block brought, written once when the
//!   block is taken (on any branch). The chain keeps only the ids; a block handed to a peer gets its bytes back from
//!   here ([`attach`]).
//! - **Waiting blobs**: the bytes of blobs that waiting transactions bring, held in memory (at most [`MAX_WAITING`])
//!   until a block brings them or their transactions go. A transaction enters the pool only with them here.
//! - **Over the mesh**, as "ac" frames: `bl {id, seq, end}` + bytes, a waiting blob sent ahead of the transactions that
//!   bring it (the transaction answer `txs` follows).

use arcade_arc::block::Block;
use arcade_arc::merkle::Hash;
use arcade_arc::params::Net;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Most bytes of waiting blobs held in memory.
pub const MAX_WAITING: usize = 64 << 20;

#[derive(Default)]
struct Waiting {
    blobs: HashMap<Hash, Vec<u8>>,
    bytes: usize,
    /// Pieces of blobs on their way from a peer: (peer, id) -> (next piece, bytes so far).
    parts: HashMap<(String, Hash), (u64, Vec<u8>)>,
}

static WAITING: Mutex<Option<Waiting>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Waiting) -> R) -> R {
    let mut g = WAITING.lock().unwrap();
    f(g.get_or_insert_with(Waiting::default))
}

static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Where the store is: `<home>/arc/<net>/blobs` (set when the chain starts).
pub fn init(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = DIR.set(dir);
}

pub fn dir() -> PathBuf {
    DIR.get().cloned().unwrap_or_default()
}

fn path(id: &Hash) -> PathBuf {
    dir().join(hex::encode(id))
}

/// Hold a waiting blob's bytes (they must be the id's). Whether it is held now.
pub fn hold(net: &Net, bytes: Vec<u8>) -> Option<Hash> {
    if bytes.is_empty() || bytes.len() > arcade_arc::blob::MAX_BLOB_BYTES {
        return None;
    }
    let id = arcade_arc::blob::id_of_bytes(net, &bytes);
    with(|w| {
        if w.blobs.contains_key(&id) {
            return Some(id);
        }
        if w.bytes + bytes.len() > MAX_WAITING {
            return None;
        }
        w.bytes += bytes.len();
        w.blobs.insert(id, bytes);
        Some(id)
    })
}

/// A blob's bytes: from the store, else from those waiting.
pub fn get(id: &Hash) -> Option<Vec<u8>> {
    std::fs::read(path(id)).ok().or_else(|| with(|w| w.blobs.get(id).cloned()))
}

/// Whether the bytes of `id` are here (stored or waiting).
pub fn here(id: &Hash) -> bool {
    path(id).exists() || with(|w| w.blobs.contains_key(id))
}

/// A block was taken: its blobs into the store (checked by the chain already), out of those waiting.
pub fn keep_block(block: &Block) {
    if block.blobs.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(dir());
    for (id, bytes) in block.blob_ids.iter().zip(&block.blobs) {
        let p = path(id);
        if !p.exists() {
            let tmp = p.with_extension("in");
            if std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
                eprintln!("arc: couldn't keep blob {}", hex::encode(&id[..8]));
            }
        }
        with(|w| {
            if let Some(b) = w.blobs.remove(id) {
                w.bytes -= b.len();
            }
        });
    }
}

/// A block as the chain keeps it, with its blobs' bytes again, to hand on. None when one isn't here (pruned).
pub fn attach(block: &Block) -> Option<Block> {
    let mut b = block.clone();
    if b.blobs.is_empty() && !b.blob_ids.is_empty() {
        b.blobs = b.blob_ids.iter().map(get).collect::<Option<Vec<_>>>()?;
    }
    Some(b)
}

/// Waiting blobs no waiting transaction names any more (and none the chain lacks) go.
pub fn forget_except(keep: &std::collections::HashSet<Hash>) {
    with(|w| {
        let gone: Vec<Hash> = w.blobs.keys().filter(|k| !keep.contains(*k)).copied().collect();
        for k in gone {
            if let Some(b) = w.blobs.remove(&k) {
                w.bytes -= b.len();
            }
        }
    });
}

/// A `bl` frame: a piece of a waiting blob from `peer`. When the last piece is in, the blob is held.
pub fn on_piece(net: &Net, peer: &str, head: &serde_json::Value, blob: &[u8]) {
    let Some(id) = head["id"].as_str().and_then(|s| hex::decode(s).ok()).and_then(|b| <Hash>::try_from(b).ok()) else { return };
    let seq = head["seq"].as_u64().unwrap_or(u64::MAX);
    let done = with(|w| {
        let key = (peer.to_string(), id);
        let e = w.parts.entry(key.clone()).or_insert((0, Vec::new()));
        if e.0 != seq || e.1.len() + blob.len() > arcade_arc::blob::MAX_BLOB_BYTES {
            w.parts.remove(&key);
            return None;
        }
        e.0 += 1;
        e.1.extend_from_slice(blob);
        if head["end"] == true {
            w.parts.remove(&key).map(|(_, b)| b)
        } else {
            None
        }
    });
    if let Some(bytes) = done {
        if hold(net, bytes) != Some(id) {
            eprintln!("arc: a blob from {} wasn't its id's, or there is no room", &peer[..peer.len().min(12)]);
        }
    }
}
