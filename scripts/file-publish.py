#!/usr/bin/env python3
"""Put a file bigger than a block on the ARCADE chain (up to 1 GB: a full-length film, Martin 2026-11-14).

  file-publish.py --sock <arcade home>/arcaded.sock --file FILM.mp4 [--type video/mp4] [--class 0|1|2] [--dry-run]

A block carries 4 MB of blobs, so the file is cut into parts of 3.9 MB, each its own blob in its own transaction
(at most 8 waiting at a time: the pool holds 64 MB of waiting blobs), then a manifest blob names the parts in order:
{"arcade": "file", "name", "type", "size", "sha256", "parts": [[blob id, length], ...]}. Every arcade then plays
http://<arcade>/content/<manifest id> as the one file, with byte ranges (a video seeks), fetching parts it doesn't
have from linked arcades and keeping them in its viewer cache. Fee: 0.01 ARCADE a kB of blobs (700 MB is about
7,000 ARCADE) plus the transactions'; about one block per part (700 MB: some 180 blocks).
"""
import argparse, hashlib, json, mimetypes, os, socket, sys, tempfile, time

PART = 3_900_000
WAITING = 8
MAX = 1 << 30


def call(sock, req, t=600):
    s = socket.socket(socket.AF_UNIX); s.settimeout(t); s.connect(sock)
    s.sendall((json.dumps(req) + "\n").encode())
    r = json.loads(s.makefile().readline())
    if not r.get("ok"):
        raise RuntimeError(r.get("error"))
    return r["result"]


def publish(sock, path, kind, cls):
    for _ in range(720):
        try:
            return call(sock, {"cmd": "arc.blob_publish", "path": path, "type": kind, "class": cls, "send": True})
        except RuntimeError as e:
            if not any(w in str(e) for w in ("not enough", "doesn't exist or is spent", "could not build", "waiting blobs")):
                raise
            time.sleep(10)
    raise SystemExit("the wallet never had enough ARCADE")


def on_chain(sock, bid):
    try:
        return call(sock, {"cmd": "arc.blob", "id": bid})["on_chain"]
    except RuntimeError:
        return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sock", required=True); ap.add_argument("--file", required=True)
    ap.add_argument("--type", default=""); ap.add_argument("--class", dest="cls", type=int, default=0)
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()
    size = os.path.getsize(a.file)
    if size > MAX:
        raise SystemExit(f"{size} bytes: a file is 1 GB at most")
    kind = a.type or mimetypes.guess_type(a.file)[0] or "application/octet-stream"
    n = max(1, -(-size // PART))
    fee = -(-size // 1000) * 0.01
    print(f"{a.file}: {size} bytes, {kind}, {n} parts; blob fees about {fee:,.2f} ARCADE, about {n} blocks", file=sys.stderr)
    if a.dry_run:
        return
    sha = hashlib.sha256()
    parts, pending = [], []
    with open(a.file, "rb") as f, tempfile.TemporaryDirectory() as tmp:
        for i in range(n):
            chunk = f.read(PART)
            sha.update(chunk)
            p = os.path.join(tmp, f"part{i}")
            open(p, "wb").write(chunk)
            while len(pending) >= WAITING:
                pending = [b for b in pending if not on_chain(a.sock, b)]
                if len(pending) >= WAITING:
                    time.sleep(5)
            r = publish(a.sock, p, "application/octet-stream", a.cls)
            os.remove(p)
            parts.append([r["blob"], len(chunk)])
            pending.append(r["blob"])
            print(f"part {i + 1}/{n}: {r['blob'][:16]}…", file=sys.stderr, flush=True)
        while pending:
            pending = [b for b in pending if not on_chain(a.sock, b)]
            time.sleep(5)
        m = {"arcade": "file", "name": os.path.basename(a.file), "type": kind, "size": size, "sha256": sha.hexdigest(), "parts": parts}
        mp = os.path.join(tmp, "manifest.json")
        json.dump(m, open(mp, "w"), separators=(",", ":"))
        r = publish(a.sock, mp, "application/vnd.arcade.file+json", a.cls)
    print(json.dumps({"file": r["blob"], "size": size, "parts": len(parts), "sha256": m["sha256"]}))


main()
