# Fast on any disk: catch-up that beats Core on a hard drive — PLAN

Status: **proposal, for Martin's review.** Measured 2026-11-15 on a test arcade syncing Bitcoin. Nothing here is
built yet; step 0 adds the measurements the later steps are judged by.

## The bar

V1.0 has to sync **faster than Bitcoin Core** (and faster than the original arcade with its Cores) **on the same
machine**, with these three setups:

| Setup | Example |
|---|---|
| hard drive only | an old PC or a NAS with one big HDD |
| SSD + hard drive | a small SSD for the system and a big HDD for chains (the most common home box) |
| SSD only | the machines V1.0 was measured on so far |

On SSDs V1.0 is already fast: mesh bootstraps run at ~1,600 blocks/s (PLAN.md §4). On a hard drive it is not. The
test arcade below would need **2–4 weeks** for Bitcoin, where Core on the same box takes **a few days** with a large
`dbcache` (to be measured in step 0). Every step below is measured with the benchmark (§5) and kept only if it
moves V1.0 ahead of Core.

## 1. What was measured

**The test arcade:** a 4-core i7-7700 with 31 GB RAM. The chain file is on a 5,400 rpm 8 TB HDD (WD Purple); the
system is on a SATA SSD. It runs v0.1.73 with `ARCADE_COIN_CACHE_MB=8192`. Bitcoin is the only thing on the HDD and
is syncing from P2P; no arcade on the mesh offers it yet.

### Bitcoin's file at height 276,322

38.4 GB in all, per table (read-only `mdbx_dbi_stat`):

| Table | Entries | Size | Role during catch-up |
|---|---|---|---|
| `core.v1.addr_log` | 298.1 M | **20.49 GB** | address rows put off until the address build; written once, read once |
| `core.v1.block_tx` | 29.7 M | **16.21 GB** | raw transactions; written once, read back by the catalog and by indexes |
| `core.v2.outputs` | 7.9 M | 0.85 GB | the coin set (spent coins are deleted); **hot** |
| `core.v1.txs` | 6.5 M | 0.42 GB | tx locator; **hot** |
| `core.v1.txs_runs` | 5.0 M | 0.18 GB | tx locator runs (TXFLUSH.md); **hot** |
| headers, hdr, height, block_meta | 0.4 M each | 0.13 GB | **hot** |
| spends, by_script, script_hist, undo, journal | 0 | 0 | not written yet (built after the catch-up) |

So the data the sync needs per block is **about 1.6 GB**. The other **36.7 GB** is bulk that is written once and
read rarely, and the bulk grows about 25× faster than the hot set. Standard scripts are stored inline with the coin
(`compress_script`), so checking a spend almost never reads `block_tx`.

### The drive and the threads

| What | Measured |
|---|---|
| The HDD | **97–110% busy**, ~260–410 reads/s of ~4 KB each (1.5–5 MB/s), ~10–40 writes/s; mdbx turns readahead off |
| Reads per thread, since a restart | `arc catalog` ~2.3 GB, the main thread ~0.8 GB, `chain btc` ~0.4 GB, `arc fraud watch` ~0.25 GB |
| `arc catalog` | `arcseg::round` measures Bitcoin's finished height ranges for segments (`measure(.., 50)`, every 2 s while there is work), reading the stored blocks back, **even with earning off** for Bitcoin (`arc.earn btc` → `on: false`) |
| Coin cache 2 → 8 GB | small effect: the cache holds new coins before a flush, but the reads come from elsewhere |
| Another Core (Bells) on the same HDD stopped | the pace went from ~1k to ~10k blocks/h for a while, then fell back as blocks grew |

What isn't pinned down yet is which tables the ~400 reads/s land in. Step 0 measures it, not guesses.

### What else points the same way

- **Core:** chainstate (coins only) in LevelDB with a big in-memory cache, and raw blocks appended to `blk*.dat`. A
  common setup is chainstate on the SSD and blocks on the HDD.
- **tbuuol's ArcadeOS `chaind`** (built on arcade-chain and libbitcoinconsensus from this repo): headers, blocks
  and undo in flat files, with only coins and the tx locator in LMDB.
- **In V1.0 everything shares one copy-on-write file per chain.** Freed pages are reused anywhere in it, so the
  bulk streams and the hot tables share pages, page cache and seeks. On an SSD that costs little; on an HDD every
  page that isn't cached is a seek.

## 2. The design: two tiers per chain

| Tier | Holds | Where |
|---|---|---|
| **Hot** (`chains/<key>/chain.mdbx`, as today) | headers, hdr, height, block_meta, outputs (scripts inline), txs, txs_runs, state, mempool, undo (last `UNDO_DEPTH` blocks), recent block bodies | the fast drive |
| **Cold** (`chains/<key>/cold/`) | old block bodies as sealed, height-ordered chunk files; addr_log as sorted run files; the history indexes the address build writes | the big drive |

### 2.1 Cold block bodies: chunks and a mover

- **Chunks:** a chunk is a range of heights below `tip − UNDO_DEPTH`, as one append-only file: the raw transactions
  in block order, plus an offset table, plus a hash over all of it. Where it fits, it should use the same cut and
  root as an ARCADE segment (arcseg, §14.13) and the V1SHARE pieces. Then sealing, serving checks and copies can read
  a chunk straight from the file instead of reading the blocks back out of mdbx.
- **The mover:** a background job that watches the fast drive's free space against a floor the operator sets. When
  space runs low, it writes the oldest bodies to a chunk on the big drive in one sequential write, checks the hash,
  then swaps the `block_tx` rows for the existing `PRUNED` stubs in one commit. Before a body is stubbed, its coins'
  scripts are made `SCRIPT_INLINE`, as Shared mode does today. Coin checks therefore never touch the cold tier.
- **Reading an old body** (explorer, ord content, `getrawtransaction`, refills for other arcades) goes to the chunk:
  one seek. `chains.refill` looks in local chunks before asking the mesh.
- **One drive only:** the same split still helps. The bulk becomes sequential files, and the hot file stays a size
  that RAM can hold.

### 2.2 addr_log as sorted runs, not mdbx rows

`addr_log` is the biggest table (20.5 GB at 276k) and is read exactly once, by the address build. Writing it as
append-only run files sorted per flush (as `txs_runs` already does for the tx index) keeps 300 M+ rows out of the
copy-on-write tree. The build then merges the runs sequentially. The address build's output (spends, by_script,
script_hist) lands in the cold tier, written in key order.

### 2.3 The coin set in memory while catching up

Today the coin cache holds coins created since the last flush, and every other spend reads `outputs` and the locator
from disk. During a catch-up, the cache should hold the **whole hot set up to a budget** (default: what the machine
can spare, as Core's `dbcache` does) and flush in sorted order. At 276k that's 1.6 GB; at Bitcoin's tip it's an
estimated 10–20 GB in V1.0's compressed form (step 0 measures it). Past the budget, the oldest coins are dropped
from memory first, since most spends hit young coins.

### 2.4 Catch-up comes first

While a chain is far behind its tip, nothing else reads its blocks back on the same drive:

- **ARCADE catalog work:** `arcseg::round` measuring and `auto_seal` sealing that chain wait until it is within a
  few hundred blocks of its tip. Segments are then cut from the chunks (2.1) rather than from mdbx.
- **Index walks and the address build** keep their current "after the catch-up" rule.
- **Fraud watch reads** of that chain's blocks wait the same way.

This step is small and should help today, before any layout change.

### 2.5 In the app

- **Chains & Coins → Drive:** two slots per chain, **Fast drive** (hot) and **Big drive** (cold), plus a floor
  for the fast drive. `chains.drives` already lists the disks; it would also say whether each is rotational
  (`/sys/block/*/queue/rotational`).
- **The default:** an SSD for the hot tier when there is one, the biggest drive for the cold tier.
- **The status line,** for example: *"hot 18 GB on SSD · cold 640 GB on HDD · moving heights 410,000–420,000"*.

## 3. What stays the same

- **Key layouts:** every key keeps the C prefix (SPLITDB.md), and the hot file is today's per-chain file.
- **Rules and checks:** nothing about consensus checking changes.
- **Copies:** V1SHARE copies, index copies and Core imports work as before; a copy of block bodies can be made
  from chunks.
- **Small chains, or a chain on one SSD:** the mover never needs to run, so nothing changes for them.

## 4. Steps

Each step is its own PR with before/after numbers from the benchmark (§5).

| Step | What | Size |
|---|---|---|
| 0 | **Measure.** Per-table page-read counters (a debug feature, or `perf` with mmap fault addresses mapped to table pages), plus the benchmark harness and baseline numbers for v0.1.73 against Core | small |
| 1 | **Catch-up comes first** (2.4): catalog measuring and sealing, fraud-watch reads and walks wait while a chain is far behind | small |
| 2 | **Coin set in memory while catching up** (2.3) | medium |
| 3 | **addr_log as sorted runs** (2.2) | medium |
| 4 | **Cold block bodies:** chunk format, mover, refill from local chunks (2.1) | large |
| 5 | **Two drive slots and the status line** in the app (2.5) | small |

## 5. The benchmark

A tool beside `arcade-split`, for example `tools/bench-sync`, run on one machine with these fixed inputs:

- **Ranges:** Bitcoin 0 → 400,000 (small blocks, then full ones) and 400,000 → 450,000 (SegWit-era blocks);
  Dogecoin and Litecoin ranges of similar weight.
- **Sources:** blocks from a local peer on the LAN (a Core node that has the range), so the network isn't what's
  measured.
- **Contenders:**
  - Bitcoin Core with `dbcache` set to the same memory budget, run twice: default `assumevalid`, and
    `assumevalid=0` (all scripts checked, as V1.0 does).
  - V1.0 at the same budget.
  - The original arcade with its Cores, where it covers the chain.
- **Disks:** the three setups in "The bar".
- **Recorded:** wall time; blocks/s per 10k-height bucket; disk reads/s and writes/s; bytes written; peak memory;
  the hot and cold sizes at the end.
- **Results:** a table in this file, one row per step. **The plan is done when V1.0 beats Core in every setup.**

## 6. Questions for Martin

1. **The chunk format:** the ARCADE segment cut and root, or a plainer format?
2. **Where bodies live:** should block bodies above `tip − UNDO_DEPTH` stay in mdbx, or should recent chunks
   also be files?
3. **Shared mode:** today it prunes bodies other arcades hold. With chunks, should a Shared arcade keep its own
   bodies in cold chunks rather than prune them?
4. **ARCADE storage proofs:** can sealing and serving checks read chunks directly (2.1), so a chain's bulk
   exists once on disk?
5. **Step 1 first:** is pausing catalog measuring during a catch-up acceptable before mainnet, or does the
   catalog need those segments early?
