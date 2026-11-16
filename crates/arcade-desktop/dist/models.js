// Models tab: shared AI models on the mesh, paid in ARCADE (arcaded llmsvc.rs, docs/LLM.md).
(() => {
  const msg = (t, bad) => say("md-msg", t, bad);
  const smsg = (t, bad) => say("md-smsg", t, bad);
  const num = (x, d = 6) => (x == null ? "—" : Number(x).toLocaleString(undefined, { maximumFractionDigits: d }));
  const who = (o) => (o.tag ? "@" + o.tag : o.seller.slice(0, 12)) + (o.here ? " (this arcade)" : "");
  let offers = [];
  let found = [];

  function record(r) {
    if (!r || !r.requests) return "new";
    const ok = r.requests - r.failed;
    const parts = [`${ok}/${r.requests} ok`];
    if (ok > 0) parts.push(`${(r.first_ms / ok / 1000).toFixed(1)} s to first word`);
    if (r.counted_out > 0 && r.billed_out > r.counted_out * 1.6) parts.push("bills more tokens than counted");
    return parts.join(", ");
  }

  function drawOffers() {
    const tb = $("md-offers");
    const rowsOut = [];
    const pick = $("md-pick");
    const was = pick.value;
    pick.replaceChildren();
    for (const o of offers) {
      for (const m of o.models) {
        const tr = document.createElement("tr");
        const cells = [m.id, who(o), m.free ? "free" : num(m.in), m.free ? "free" : num(m.out), o.here ? "—" : num(o.credit), o.here ? "yours" : record(o.record)];
        cells.forEach((c, i) => { const td = document.createElement("td"); td.textContent = c; if (i >= 2 && i <= 4) td.className = "num"; tr.append(td); });
        const td = document.createElement("td");
        if (!o.here && !m.free) {
          const amt = document.createElement("input"); amt.type = "number"; amt.min = "0.01"; amt.step = "0.1"; amt.value = "1"; amt.style.width = "5em";
          const buy = document.createElement("button"); buy.className = "small"; buy.textContent = "Buy credit";
          buy.onclick = async () => {
            buy.disabled = true;
            try {
              const r = await api("llm.topup", { seller: o.seller, amount: Number(amt.value) });
              msg(`Paid ${r.paid} ARCADE to ${who(o)} (fee ${r.fee}). It is credited once the payment is in a block, about 2.5 minutes.`);
            } catch (e) { msg(e.message, true); }
            buy.disabled = false;
          };
          const back = document.createElement("button"); back.className = "small"; back.textContent = "Credit back";
          back.hidden = !o.refunds || !(o.credit > 0);
          back.onclick = async () => {
            if (!back.dataset.sure) { back.dataset.sure = 1; back.textContent = "Credit back: sure?"; return; }
            try { const r = await api("llm.refund", { seller: o.seller }); msg(`${who(o)} paid ${r.refunded} ARCADE back.`); draw(); } catch (e) { msg(e.message, true); }
          };
          td.append(amt, " ", buy, " ", back);
        }
        tr.append(td);
        rowsOut.push(tr);
        const opt = document.createElement("option");
        opt.value = JSON.stringify({ seller: o.seller, model: m.id });
        opt.textContent = `${m.id} — ${who(o)}${m.free ? " (free)" : ""}`;
        pick.append(opt);
      }
    }
    if (!rowsOut.length) tb.innerHTML = '<tr><td colspan="7" class="s">No models on offer yet. Arcades that share one show up here within a few minutes.</td></tr>';
    else tb.replaceChildren(...rowsOut);
    if ([...pick.options].some((x) => x.value === was)) pick.value = was;
  }

  async function drawKeys() {
    const k = await api("llm.keys");
    $("md-base").textContent = arcadeUrl(k.base_url);
    const tb = $("md-keys");
    if (!k.keys.length) { tb.innerHTML = '<tr><td colspan="3" class="s">No keys yet.</td></tr>'; return; }
    tb.replaceChildren(...k.keys.map((x) => {
      const tr = document.createElement("tr");
      [x.name || x.id, new Date(x.created * 1000).toLocaleString()].forEach((c) => { const td = document.createElement("td"); td.textContent = c; tr.append(td); });
      const td = document.createElement("td"); const b = document.createElement("button"); b.className = "small"; b.textContent = "Revoke";
      b.onclick = async () => { if (!b.dataset.sure) { b.dataset.sure = 1; b.textContent = "Revoke: sure?"; return; } await api("llm.key_drop", { id: x.id }); drawKeys(); };
      td.append(b); tr.append(td); return tr;
    }));
  }

  async function drawAuto() {
    const a = await api("llm.auto", {});
    $("md-auto").checked = !!a.on; $("md-floor").value = a.floor; $("md-amt").value = a.amount; $("md-cap").value = a.daily_cap;
  }

  function drawMine(st) {
    const shared = new Set(st.models || []);
    const prices = st.prices || {};
    const ids = [...new Set([...(found.map((m) => m.id)), ...shared])];
    const tb = $("md-mine");
    if (!ids.length) { tb.innerHTML = '<tr><td colspan="4" class="s">Find the server\'s models first.</td></tr>'; return; }
    tb.replaceChildren(...ids.map((id) => {
      const tr = document.createElement("tr"); tr.dataset.id = id;
      const p = prices[id] || {};
      tr.innerHTML = `<td><input type="checkbox" class="md-share"></td><td></td><td class="num"><input type="number" min="0" step="0.1" class="md-in" style="width:7em"></td><td class="num"><input type="number" min="0" step="0.1" class="md-out" style="width:7em"></td>`;
      tr.children[1].textContent = id;
      tr.querySelector(".md-share").checked = shared.has(id);
      tr.querySelector(".md-in").value = p.in ?? 0; tr.querySelector(".md-out").value = p.out ?? 0;
      return tr;
    }));
  }

  async function drawSelling(scan) {
    const st = await api("llm.status");
    if (scan || !found.length) found = st.found || [];
    $("md-url").value = $("md-url").value || st.url || "";
    $("md-key").placeholder = st.has_key ? "Server key (kept; type to change)" : "Server key (if it has one)";
    $("md-atonce").value = st.at_once; $("md-maxout").value = st.max_out; $("md-refunds").checked = st.refunds; $("md-on").checked = st.on;
    drawMine(st);
    const e = st.earned || {};
    $("md-earned").textContent = st.selling
      ? `Sharing: ${e.requests || 0} replies sold, ${num(e.arcade)} ARCADE earned (${num(e.day_arcade)} today), ${e.buyers || 0} buyer${e.buyers === 1 ? "" : "s"} holding ${num(e.credit_held)} ARCADE of credit`
      : (st.found_error ? st.found_error : "Not sharing.");
  }

  async function draw() {
    try {
      offers = await api("llm.offers");
      drawOffers();
    } catch (e) { msg(e.message, true); }
    drawKeys().catch((e) => msg(e.message, true));
    drawAuto().catch(() => {});
    drawSelling(false).catch((e) => smsg(e.message, true));
  }

  $("md-send").onclick = async () => {
    const v = $("md-pick").value;
    const q = $("md-ask").value.trim();
    if (!v || !q) return;
    const { seller, model } = JSON.parse(v);
    const out = $("md-reply");
    out.hidden = false; out.textContent = "…"; $("md-cost").textContent = "";
    $("md-send").disabled = true;
    try {
      const r = await api("llm.chat", { seller, model, messages: [{ role: "user", content: q }] });
      out.textContent = r.content || "(no text)";
      $("md-cost").textContent = `${r.usage.tokens_in} in, ${r.usage.tokens_out} out: ${num(r.cost)} ARCADE, ${num(r.left)} left, ${(r.ms / 1000).toFixed(1)} s`;
      draw();
    } catch (e) { out.textContent = e.message; }
    $("md-send").disabled = false;
  };

  $("md-knew").onclick = async () => {
    try {
      const r = await api("llm.key_new", { name: $("md-kname").value });
      const out = $("md-kout");
      out.replaceChildren();
      const b = document.createElement("b"); b.textContent = "Your new key: copy it now, it isn't shown again.";
      const c = document.createElement("code"); c.textContent = r.key;
      const copy = document.createElement("button"); copy.className = "small"; copy.textContent = "Copy";
      copy.onclick = () => navigator.clipboard?.writeText(r.key).then(() => { copy.textContent = "Copied"; });
      out.append(b, document.createElement("br"), c, " ", copy);
      out.hidden = false; $("md-kname").value = "";
      drawKeys();
    } catch (e) { msg(e.message, true); }
  };

  $("md-autosave").onclick = async () => {
    try {
      await api("llm.auto", { on: $("md-auto").checked, floor: Number($("md-floor").value), amount: Number($("md-amt").value), daily_cap: Number($("md-cap").value) });
      msg("Auto top-up saved.");
    } catch (e) { msg(e.message, true); }
  };

  $("md-scan").onclick = async () => {
    try {
      const args = { url: $("md-url").value.trim() };
      if ($("md-key").value) args.key = $("md-key").value;
      const r = await api("llm.scan", args);
      found = r.models;
      drawMine(await api("llm.status").then((st) => ({ ...st, found })));
      smsg(`Found ${found.length} model${found.length === 1 ? "" : "s"}. Tick the ones to share and set prices (0 = free).`);
    } catch (e) { smsg(e.message, true); }
  };

  $("md-save").onclick = async () => {
    const models = []; const prices = {};
    document.querySelectorAll("#md-mine tr[data-id]").forEach((tr) => {
      const id = tr.dataset.id;
      prices[id] = { in: Number(tr.querySelector(".md-in").value || 0), out: Number(tr.querySelector(".md-out").value || 0) };
      if (tr.querySelector(".md-share").checked) models.push(id);
    });
    const args = { on: $("md-on").checked, url: $("md-url").value.trim(), models, prices, at_once: Number($("md-atonce").value), max_out: Number($("md-maxout").value), refunds: $("md-refunds").checked };
    if ($("md-key").value) args.key = $("md-key").value;
    try {
      await api("llm.set", args);
      $("md-key").value = "";
      smsg(args.on ? "Saved: your offer is going out to the mesh." : "Saved.");
      draw();
    } catch (e) { smsg(e.message, true); }
  };

  document.querySelector('nav button[data-tab="models"]').addEventListener("click", draw);
  setInterval(() => { if ($("models").classList.contains("on")) api("llm.offers").then((o) => { offers = o; drawOffers(); }).catch(() => {}); }, 30000);
})();
