// Start, stop, a terminal, and installing a chain. Stop and the terminal go through the local
// API (this computer's socket or the cookie). Start is the app's own command: the node is down,
// so it can't be asked. Nothing here runs a command the page typed.
(() => {
  const MODES = [
    ["full", "Full", "Blocks, coins, and the indexes you tick. Serves the mesh."],
    ["shared", "Shared", "Coins, plus your share of the blocks. The rest is fetched when needed."],
    ["chain", "Blockchain only", "Blocks and coins. Addresses if you tick it. No protocol indexes."],
    ["light", "Light", "Headers only, about 70 MB. No blocks, coins or indexes."],
  ];
  // Same indexes as Chains & Coins. Nothing is ticked until you tick it.
  const INDEXES = [
    ["addresses", "Addresses", "every address's history and balance", true],
    ["ledger", "Arcade ledger", "arcade tokens, exchange, arcade NFTs, @tags", true],
    ["profiles", "Profiles", "published keys, names and linked wallets", true],
    ["ord", "Ord index", "inscriptions and content served from the chain", true, "ord"],
    ["names", "Names", ".pepe / .doge names (needs ord)", true, "ord"],
    ["maps", "Maps", "block maps (needs ord)", true, "ord"],
    ["tokens", "Tokens", "PRC-20 / DRC-20 (needs ord)", true, "ord"],
    ["collections", "Collections", "NFT collections published on the chain (needs ord)", true, "ord"],
    ["dogefeed", "DogeFeed", "D:// posts, comments and tips", true, "dogefeed"],
    ["runes", "Runes", "coming: Bitcoin's fungible tokens", false, "runes"],
    ["dunes", "Dunes", "coming: Dogecoin's runes-style tokens", false, "dunes"],
  ];
  const NEEDS_ORD = ["names", "maps", "tokens", "collections"];
  const gb = (b) => `${(Number(b || 0) / 1e9).toFixed(b >= 1e10 ? 0 : 1)} GB`;

  let mode = "full", chains = [], drives = [], drivesAt = 0, timer = null;

  const on = (id, fn) => { const el = $(id); if (el) el.onclick = fn; };
  const optionsOpen = () => $("options") && $("options").classList.contains("on") && !document.hidden;
  const formBusy = () => {
    const a = document.activeElement;
    return !!(a && $("o-node") && $("o-node").contains(a) && (a.tagName === "SELECT" || a.tagName === "INPUT"));
  };

  function setRunning(up) {
    for (const id of ["node-start", "o-start"]) { const b = $(id); if (b) b.hidden = !!up; }
    for (const id of ["node-stop", "o-stop"]) {
      const b = $(id);
      if (!b) continue;
      b.hidden = !up;
      if (up && b.dataset.stopping !== "1") { b.disabled = false; b.textContent = "Stop"; }
    }
  }
  window.arcadeNodeSeen = (up) => setRunning(!!up);

  function stops() { return ["node-stop", "o-stop"].map($).filter(Boolean); }

  async function stopNode() {
    if (!confirm("Stop the node? It writes its caches first, the same as arcade stop, then exits.")) return;
    for (const b of stops()) { b.dataset.stopping = "1"; b.disabled = true; b.textContent = "Stopping…"; }
    try {
      await api("node.stop");
      say("o-node-msg", "Stopping: writing the coin caches, then the service exits.");
    } catch (e) {
      for (const b of stops()) { b.dataset.stopping = ""; b.disabled = false; b.textContent = "Stop"; }
      say("o-node-msg", e.message || String(e), true);
    }
  }

  async function startNode() {
    if (IN_BROWSER) {
      openSettings();
      say("o-node-msg", "This page is served by the node, so it can't start itself. Open the Arcade app, or run: systemctl --user start arcaded", true);
      return;
    }
    try {
      const note = await invoke("node_start");
      say("o-node-msg", note || "Started.");
      setRunning(true);
    } catch (e) {
      say("o-node-msg", e.message || String(e), true);
    }
  }

  async function openTerm() {
    try {
      if (IN_BROWSER) await api("node.terminal");
      else await invoke("open_cli");
      say("o-node-msg", "Opened a terminal running the arcade CLI.");
    } catch (e) {
      say("o-node-msg", e.message || String(e), true);
    }
  }

  function openSettings() {
    const b = document.querySelector('nav button[data-tab="options"]');
    if (b && !$("options").classList.contains("on")) b.click();
    else refreshInstall();
    const n = $("o-node");
    if (n) n.scrollIntoView({ block: "start" });
  }

  on("node-stop", stopNode);
  on("o-stop", stopNode);
  on("node-start", startNode);
  on("o-start", startNode);
  on("o-term", openTerm);
  on("node-set", openSettings);

  function checkedIdx() {
    return new Set([...($("o-idx") ? $("o-idx").querySelectorAll("input:checked") : [])].map((i) => i.dataset.k));
  }

  function paintModes(c) {
    const lightOk = c && c.key === "btc";
    if (mode === "light" && !lightOk) mode = "full";
    $("o-modes").replaceChildren(...MODES.map(([k, t, d]) => {
      const ok = k !== "light" || lightOk;
      const label = h("label", { class: "mode" + (k === mode ? " sel" : "") + (ok ? "" : " off") },
        h("input", { type: "radio", name: "o-mode", checked: k === mode, disabled: !ok }),
        h("b", {}, t),
        h("small", {}, k === "light" && !lightOk ? "Bitcoin only: headers only." : d));
      if (ok) label.onclick = () => { mode = k; paintModes(c); paintIdx(c); };
      return label;
    }));
  }

  function paintIdx(c) {
    const keep = checkedIdx();
    const light = mode === "light", noIdx = mode === "chain";
    const rows = INDEXES.filter((row) => {
      const needs = row[4];
      return (needs !== "runes" || c.runes) && (needs !== "dunes" || c.dunes) && (needs !== "dogefeed" || c.dogefeed);
    });
    $("o-idx").replaceChildren(...rows.map(([k, t, d, ok, needs]) => {
      const can = ok && !light && (!noIdx || k === "addresses") && (needs !== "ord" || c.ord) && (k !== "tokens" || c.tokens);
      const label = h("label", { class: can ? "" : "off" },
        h("input", { type: "checkbox", "data-k": k, disabled: !can, checked: can && keep.has(k) }),
        h("b", {}, k === "tokens" && c.tokens ? `${c.tokens} tokens` : t),
        h("small", {}, can || !ok ? d : "not available on this chain yet"));
      const box = label.querySelector("input");
      box.onchange = () => {
        const all = $("o-idx");
        if (box.checked && NEEDS_ORD.includes(k)) { const ord = all.querySelector('input[data-k="ord"]'); if (ord && !ord.disabled) ord.checked = true; }
        if (!box.checked && k === "ord") NEEDS_ORD.forEach((r) => { const x = all.querySelector(`input[data-k="${r}"]`); if (x) x.checked = false; });
      };
      return label;
    }));
  }

  function paintDrive() {
    const sel = $("o-drive");
    const cur = sel.value || "home";
    sel.replaceChildren(...drives.map((d) => h("option", { value: d.id, disabled: d.ok === false },
      `${d.home ? "This arcade's drive" : (d.label || d.id)} · ${gb(d.free)} free`)));
    if ([...sel.options].some((o) => o.value === cur && !o.disabled)) sel.value = cur;
    else if ([...sel.options].some((o) => o.value === "home")) sel.value = "home";
  }

  function paintChains() {
    const sel = $("o-chain");
    const cur = sel.value;
    const choices = chains.filter((c) => !c.installed);
    sel.replaceChildren(...choices.map((c) => h("option", { value: c.key }, c.name + (c.queued ? ` (queued #${c.queued})` : ""))));
    if (choices.some((c) => c.key === cur)) sel.value = cur;
  }

  function showFor(c) {
    const arcade = !!(c && c.arcade);
    $("o-modes").hidden = !c || arcade;
    $("o-idx").hidden = !c || arcade;
    $("o-drive").hidden = !c || arcade;
    if (!c || arcade) { if (arcade) { $("o-modes").replaceChildren(); $("o-idx").replaceChildren(); } return; }
    paintModes(c);
    paintIdx(c);
  }

  async function loadDrives() {
    if (Date.now() - drivesAt < 30000 && drives.length) return drives;
    try { drives = (await api("chains.drives")) || []; drivesAt = Date.now(); } catch { drives = []; }
    return drives;
  }

  async function paintMoves() {
    const box = $("o-moves");
    if (box.contains(document.activeElement)) return;
    const installed = chains.filter((c) => c.installed && !c.home && !c.arcade);
    if (!installed.length) {
      box.replaceChildren(h("p", { class: "opt" }, "No extra chain is installed here yet. The home chain stays on this arcade's drive."));
      return;
    }
    const rows = [];
    for (const c of installed) {
      let st = null;
      try { st = await api("chains.status", { key: c.key }); } catch { continue; }
      if (!st || !st.place) continue;
      const name = st.name || c.name;
      const onDrive = st.place.drive === "home" ? "this arcade's drive" : (st.place.drive || "another drive");
      const mv = st.move;
      if (mv && mv.finished == null && mv.phase) {
        const prog = mv.phase === "stopping" ? "stopping its sync" : `${gb(mv.bytes_done)} of ${gb(mv.bytes)}`;
        rows.push(h("div", { class: "addrow" }, h("div", {}, h("b", {}, `${name}: moving to ${mv.to || "another drive"}`), h("small", {}, prog))));
        continue;
      }
      const list = drives.filter((d) => d.ok !== false && d.id !== st.place.drive);
      const sel = h("select", { title: `Move ${name}` });
      for (const d of list) sel.append(h("option", { value: d.id }, `${d.home ? "This arcade's drive" : (d.label || d.id)} · ${gb(d.free)} free`));
      const b = h("button", { type: "button", class: "small", disabled: !list.length }, "Move");
      b.onclick = async () => {
        const d = list.find((x) => x.id === sel.value);
        if (!d) return;
        if (!confirm(`Move ${name} to ${d.home ? "this arcade's drive" : (d.label || d.id)}? Its sync pauses while the file is copied.`)) return;
        b.disabled = true;
        try {
          await api("chains.move", { key: c.key, drive: sel.value });
          say("o-node-msg", `Moving ${name}…`);
          refreshInstall();
        } catch (e) { b.disabled = false; say("o-node-msg", e.message || String(e), true); }
      };
      rows.push(h("div", { class: "addrow" },
        h("div", {}, h("b", {}, `${name} · stored on ${onDrive}`), h("small", {}, `${st.place.dir || ""} · ${gb(st.place.bytes)}`.trim())),
        list.length ? h("div", {}, sel, " ", b) : h("small", {}, "No other drive to move it to.")));
    }
    if (!rows.length) rows.push(h("p", { class: "opt" }, "Installed chains show here once their place on disk is known."));
    box.replaceChildren(...rows);
  }

  async function refreshInstall() {
    if (!optionsOpen()) return;
    let list;
    try { list = await api("chains.list"); } catch { return; }
    if (!Array.isArray(list)) return;
    chains = list;
    if (!formBusy()) {
      await loadDrives();
      paintChains();
      paintDrive();
      showFor(chains.find((c) => c.key === $("o-chain").value));
    }
    await paintMoves();
  }

  if ($("o-chain")) $("o-chain").onchange = () => showFor(chains.find((c) => c.key === $("o-chain").value));

  on("o-install", async () => {
    const c = chains.find((x) => x.key === $("o-chain").value);
    if (!c) { say("o-node-msg", "Choose a chain first.", true); return; }
    const btn = $("o-install");
    btn.disabled = true;
    try {
      const req = c.arcade ? { key: c.key } : {
        key: c.key,
        mode,
        with: [...$("o-idx").querySelectorAll("input:checked")].map((i) => i.dataset.k),
        drive: $("o-drive").value || "home",
      };
      const st = await api("chains.add", req);
      say("o-node-msg", st && st.queued
        ? `${c.name} waits its turn (#${st.position}${st.behind ? ` behind ${st.behind}` : ""}). It installs by itself once the chain ahead has caught up.`
        : `Installing ${c.name}.`);
      refreshInstall();
    } catch (e) {
      say("o-node-msg", e.message || String(e), true);
    } finally {
      btn.disabled = false;
    }
  });

  function arm() {
    if (timer) return;
    timer = setInterval(() => { if (optionsOpen()) refreshInstall(); }, 15000);
  }
  document.querySelector('nav button[data-tab="options"]')?.addEventListener("click", () => { arm(); refreshInstall(); });
  document.addEventListener("visibilitychange", () => { if (optionsOpen()) refreshInstall(); });

  // The first refresh may have finished before this file loaded.
  api("status").then(() => window.arcadeNodeSeen(true)).catch(() => window.arcadeNodeSeen(false));
})();
