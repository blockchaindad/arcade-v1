#!/usr/bin/env python3
"""Import a GitHub repository's issues, pull requests, comments and reviews into an @tag repository on the ARCADE
chain (docs/ARCADECHAIN.md §15 step 10).

  github-import.py --sock <arcade home>/arcaded.sock --github owner/repo --to @tag/name [--from-number N] [--limit N]
                   [--cache FILE]
  github-import.py --github owner/repo --cache FILE --fetch-only     read GitHub into FILE, write nothing on chain

The code itself goes first with git-publish.py --to @tag/name (same commit ids as GitHub). This reads GitHub with the
`gh` command (signed in, or public repositories) and writes, in GitHub's number order, each issue and pull request
(ledger.item_open), its comments and reviews (ledger.item_comment) and its end state (ledger.item_state: a merged
pull request is recorded as merged at the repository's manifest now). Every text keeps where it came from:
{"github": {"number", "author", "created_at", "url"}}; the ARCADE author is the importing wallet's @tag, so the import
needs the repository's owner or a maintainer for reviews and merges. Prints the number map as JSON lines.
"""
import argparse, json, os, socket, subprocess, sys, time


def call(sock, req, t=300):
    s = socket.socket(socket.AF_UNIX); s.settimeout(t); s.connect(sock)
    s.sendall((json.dumps(req) + "\n").encode())
    r = json.loads(s.makefile().readline())
    if not r.get("ok"):
        raise RuntimeError(r.get("error"))
    return r["result"]


def send(sock, req):
    """A record, waiting while the wallet's coins are tied up in transactions not yet in a block."""
    for _ in range(360):
        try:
            return call(sock, req)
        except RuntimeError as e:
            if not any(w in str(e) for w in ("not enough", "could not build", "spent by another", "doesn't exist or is spent")):
                raise
            time.sleep(5)
    raise SystemExit("the wallet never had enough ARCADE")


CACHE = {}


def gh(path):
    if path in CACHE:
        return CACHE[path]
    r = subprocess.run(["gh", "api", "--paginate", path], capture_output=True, text=True)
    if r.returncode != 0:
        raise SystemExit(f"gh api {path}: {r.stderr.strip()}")
    # --paginate joins the pages' arrays back to back.
    out = []
    dec = json.JSONDecoder()
    s, i = r.stdout.strip(), 0
    while i < len(s):
        v, j = dec.raw_decode(s, i)
        out.extend(v if isinstance(v, list) else [v])
        i = j
        while i < len(s) and s[i] in " \n\r\t":
            i += 1
    CACHE[path] = out
    return out


def origin(x, kind):
    return {"github": {"kind": kind, "number": x.get("number"), "author": (x.get("user") or {}).get("login"),
                       "created_at": x.get("created_at") or x.get("submitted_at"), "url": x.get("html_url")}}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sock", default=""); ap.add_argument("--github", required=True); ap.add_argument("--to", required=True)
    ap.add_argument("--from-number", type=int, default=1); ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--cache", default=""); ap.add_argument("--fetch-only", action="store_true")
    a = ap.parse_args()
    gr = a.github
    if a.cache and os.path.exists(a.cache):
        CACHE.update(json.load(open(a.cache)))
    if a.fetch_only:
        # Everything the import reads, into the cache (for an arcade with no way to GitHub).
        for x in gh(f"repos/{gr}/issues?state=all&per_page=100"):
            if x.get("comments"):
                gh(f"repos/{gr}/issues/{x['number']}/comments?per_page=100")
            if "pull_request" in x:
                call_gh_one(f"repos/{gr}/pulls/{x['number']}")
                gh(f"repos/{gr}/pulls/{x['number']}/reviews?per_page=100")
                gh(f"repos/{gr}/pulls/{x['number']}/comments?per_page=100")
        json.dump(CACHE, open(a.cache, "w"))
        print(f"{len(CACHE)} GitHub answers in {a.cache}", file=sys.stderr)
        return
    call(a.sock, {"cmd": "ledger.repo", "name": a.to})
    items = sorted((x for x in gh(f"repos/{gr}/issues?state=all&per_page=100") if x["number"] >= a.from_number), key=lambda x: x["number"])
    if a.limit:
        items = items[:a.limit]
    print(f"{len(items)} issues and pull requests from {gr} into {a.to}", file=sys.stderr)
    for x in items:
        pull = "pull_request" in x
        body = {"repo": a.to, "kind": "pull" if pull else "issue", "title": x["title"], "body": x.get("body") or "",
                "extra": origin(x, "pull" if pull else "issue")}
        pr = None
        if pull:
            pr = call_gh_one(f"repos/{gr}/pulls/{x['number']}")
            body["head"] = pr["head"]["sha"]
            body["from"] = pr["head"]["label"]
            body["extra"]["github"]["base"] = pr["base"]["ref"]
        before = {i["number"] for i in call(a.sock, {"cmd": "ledger.items", "repo": a.to})["items"]}
        send(a.sock, {"cmd": "ledger.item_open", **body})
        # Its ARCADE number: the next one, once the record is in a block (comments name it).
        n = None
        for _ in range(240):
            got = [i for i in call(a.sock, {"cmd": "ledger.items", "repo": a.to})["items"] if i["number"] not in before and (i["text"] or {}).get("extra", {}).get("github", {}).get("number") == x["number"]]
            if got:
                n = got[0]["number"]; break
            time.sleep(2)
        if n is None:
            raise SystemExit(f"#{x['number']} never showed up on the chain")
        events = []
        if x.get("comments"):
            events += [("comment", c, None) for c in gh(f"repos/{gr}/issues/{x['number']}/comments?per_page=100")]
        if pull:
            for rv in gh(f"repos/{gr}/pulls/{x['number']}/reviews?per_page=100"):
                verdict = {"APPROVED": "approve", "CHANGES_REQUESTED": "changes"}.get(rv.get("state"))
                events.append(("review", rv, verdict))
            events += [("review_comment", c, None) for c in gh(f"repos/{gr}/pulls/{x['number']}/comments?per_page=100")]
        events.sort(key=lambda e: e[1].get("created_at") or e[1].get("submitted_at") or "")
        for kind, c, verdict in events:
            text = c.get("body") or ""
            if kind == "review_comment":
                text = f"{c.get('path')}:{c.get('line') or c.get('original_line')}\n{text}"
            if not text and not verdict:
                continue
            send(a.sock, {"cmd": "ledger.item_comment", "repo": a.to, "number": n, "body": text, "review": verdict or "", "extra": origin(c, kind)})
        if x["state"] == "closed":
            merged = bool(pr and pr.get("merged_at"))
            st = {"cmd": "ledger.item_state", "repo": a.to, "number": n, "state": "merged" if merged else "closed"}
            if merged:
                st["manifest"] = call(a.sock, {"cmd": "ledger.repo", "name": a.to})["manifest"]
            send(a.sock, st)
        print(json.dumps({"github": x["number"], "arcade": n, "kind": body["kind"], "events": len(events), "state": x["state"]}), flush=True)


def call_gh_one(path):
    if path in CACHE:
        return CACHE[path]
    r = subprocess.run(["gh", "api", path], capture_output=True, text=True)
    if r.returncode != 0:
        raise SystemExit(f"gh api {path}: {r.stderr.strip()}")
    CACHE[path] = json.loads(r.stdout)
    return CACHE[path]


main()
