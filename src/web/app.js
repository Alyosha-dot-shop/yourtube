// YourTube web: card menus, actions, background refreshes and the player. No frameworks.
"use strict";

const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => Array.from(el.querySelectorAll(s));
const incognito = () => document.body.classList.contains("incognito");

let toastTimer;
function toast(msg) {
  const t = $("#toast");
  if (!t || !msg) return;
  t.textContent = msg;
  t.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.remove("show"), 3200);
}

async function post(url, data, opts = {}) {
  const r = await fetch(url, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(data),
    ...opts,
  });
  if (r.status === 401) { location.href = "/login?next=" + encodeURIComponent(location.pathname + location.search); return {}; }
  let j = {};
  try { j = await r.json(); } catch (_) { /* empty answer */ }
  if (!r.ok && !j.error) j.error = "The server said " + r.status;
  return j;
}

// A video's details, from a card or from the watch page.
function videoOf(el) {
  const d = el.dataset;
  return { id: d.id, title: d.title || "", channel: d.channel || "", cid: d.cid || "", len: d.len || "", pl: d.pl || "" };
}

async function act(name, v, el) {
  if (name === "copy") return copyLink(v.id);
  const j = await post("/api/action", { act: name, ...v });
  if (j.error) return toast(j.error);
  toast(j.msg);
  const card = el && el.closest(".card");
  if (j.hide && card) card.remove();
  if (j.hide_channel) $$(".card").filter(c => c.dataset.cid === j.hide_channel).forEach(c => c.remove());
  if (name === "watch_later") {
    $$(`[data-id="${v.id}"]`).forEach(c => (c.dataset.wl = j.on ? "1" : ""));
    $$('[data-act="watch_later"]').forEach(b => { b.setAttribute("aria-pressed", String(!!j.on)); b.textContent = j.on ? "Saved to Watch later" : "Watch later"; });
  }
  if (name === "watched" || name === "unwatched") {
    $$(`[data-id="${v.id}"]`).forEach(c => { c.dataset.watched = j.on ? "1" : ""; c.classList.toggle("done", !!j.on); });
    $$('[data-act="watched"],[data-act="unwatched"]').forEach(b => { b.dataset.act = j.on ? "unwatched" : "watched"; b.textContent = j.on ? "Mark as unwatched" : "Mark as watched"; });
  }
}

function copyLink(id) {
  const url = "https://www.youtube.com/watch?v=" + id;
  const done = () => toast("Link copied");
  if (navigator.clipboard && window.isSecureContext) return navigator.clipboard.writeText(url).then(done, () => prompt("Copy the link:", url));
  const ta = document.createElement("textarea");
  ta.value = url; ta.setAttribute("readonly", ""); ta.style.position = "fixed"; ta.style.opacity = "0";
  document.body.appendChild(ta); ta.select();
  let ok = false;
  try { ok = document.execCommand("copy"); } catch (_) { /* not allowed */ }
  ta.remove();
  ok ? done() : prompt("Copy the link:", url);
}

// The ⋮ menu: a bottom sheet on phones, a small dialog on wider screens.
function openSheet(card) {
  const sheet = $("#sheet");
  const v = videoOf(card);
  const d = card.dataset;
  $(".sheet-title", sheet).textContent = v.title;
  const box = $(".sheet-actions", sheet);
  box.replaceChildren();
  const add = (label, fn) => {
    const b = document.createElement("button");
    b.type = "button"; b.textContent = label;
    b.addEventListener("click", () => { sheet.close(); fn(); });
    box.appendChild(b);
  };
  if (!incognito()) {
    add(d.wl ? "Remove from Watch later" : "Save to Watch later", () => act("watch_later", v, card));
    if (d.pl) add("Remove from this playlist", () => act("remove", v, card));
    add(d.watched ? "Mark as unwatched" : "Mark as watched", () => act(d.watched ? "unwatched" : "watched", v, card));
    add("More like this", () => act("more", v, card));
    add("Not interested", () => act("not_interested", v, card));
    if (v.cid) add(`Don't show ${v.channel || "this channel"}`, () => act("block", v, card));
  }
  if (v.cid) add(`Go to ${v.channel || "channel"}`, () => (location.href = "/channel/" + v.cid));
  add("Copy link", () => copyLink(v.id));
  if (typeof sheet.showModal === "function") sheet.showModal(); else sheet.setAttribute("open", "");
}

document.addEventListener("click", async e => {
  const t = e.target.closest("button, a");
  if (!t) return;
  if (t.classList.contains("more")) { e.preventDefault(); return openSheet(t.closest(".card")); }
  if (t.classList.contains("sheet-close")) { e.preventDefault(); return $("#sheet").close(); }
  if (t.dataset.act) {
    const w = t.closest(".watch");
    if (w) { e.preventDefault(); return act(t.dataset.act, videoOf(w), t); }
  }
  if (t.dataset.sub) {
    e.preventDefault();
    const on = t.getAttribute("aria-pressed") !== "true";
    const j = await post("/api/action", { act: on ? "subscribe" : "unsubscribe", cid: t.dataset.sub, channel: t.dataset.name || "" });
    if (j.error) return toast(j.error);
    t.setAttribute("aria-pressed", String(on));
    t.textContent = on ? "Subscribed" : "Subscribe";
    t.classList.toggle("primary", !on);
    return toast(j.msg);
  }
  if (t.dataset.unblock) {
    e.preventDefault();
    const j = await post("/api/action", { act: "unblock", cid: t.dataset.unblock });
    if (j.error) return toast(j.error);
    t.closest("li").remove();
    return toast(j.msg);
  }
  if (t.dataset.refresh) {
    e.preventDefault();
    t.disabled = true;
    const j = await post("/api/refresh", { what: t.dataset.refresh });
    if (j.reload) return location.reload();
    if (j.error) { t.disabled = false; return toast(j.error); }
    const st = $(`.status[data-job="${t.dataset.refresh}"]`);
    if (st) { st.textContent = j.message || "Starting…"; st.dataset.running = "1"; }
    poll(t.dataset.refresh);
  }
});

$("#sheet")?.addEventListener("click", e => { if (e.target.id === "sheet") e.target.close(); });

// Background refreshes: show progress, reload when done.
function poll(what) {
  const st = $(`.status[data-job="${what}"]`);
  const btn = $(`[data-refresh="${what}"]`);
  const tick = async () => {
    let j;
    try { j = await (await fetch("/api/status?what=" + what, { credentials: "same-origin" })).json(); } catch (_) { return setTimeout(tick, 3000); }
    if (st) st.textContent = j.message || "";
    if (j.running) return setTimeout(tick, 1200);
    if (st) st.dataset.running = "";
    if (btn) btn.disabled = false;
    if (j.error) return toast(j.message);
    location.reload();
  };
  setTimeout(tick, 800);
}
$$('.status[data-running="1"]').forEach(s => poll(s.dataset.job));

// The watch page.
const watch = $(".watch");
if (watch) {
  const v = $("#video");
  const id = watch.dataset.id;
  const start = parseFloat(watch.dataset.start) || 0;
  const save = watch.dataset.save === "1";
  const msg = $(".player-msg");
  let lastSaved = -1;

  if (start > 0) v.addEventListener("loadedmetadata", () => { if (v.currentTime < 1) v.currentTime = start; }, { once: true });

  const saveNow = (keepalive = false) => {
    if (!save || !v.currentTime || Math.abs(v.currentTime - lastSaved) < 1) return;
    lastSaved = v.currentTime;
    const d = videoOf(watch);
    post("/api/progress", { id, position: v.currentTime, length: isFinite(v.duration) ? v.duration : null, title: d.title, channel: d.channel, cid: d.cid }, { keepalive }).catch(() => {});
  };
  setInterval(() => { if (!v.paused && !v.ended) saveNow(); }, 5000);
  v.addEventListener("pause", () => saveNow());
  v.addEventListener("ended", () => saveNow());
  addEventListener("pagehide", () => saveNow(true));

  // Title, channel and description from yt-dlp (it is asked anyway to find the stream).
  fetch("/api/info/" + id, { credentials: "same-origin" }).then(r => r.json()).then(j => {
    if (!j.ok) {
      if (j.error) { msg.textContent = "Can't play this video: " + j.error; msg.hidden = false; }
      return;
    }
    if (j.html) $(".v-info").innerHTML = j.html;
    Object.assign(watch.dataset, { title: j.title || "", channel: j.channel || "", cid: j.channel_id || "", len: j.length ? Math.round(j.length) : "" });
    if (j.title) document.title = j.title + " · YourTube";
  }).catch(() => {});

  v.addEventListener("error", () => {
    if (msg.hidden) { msg.textContent = "The video didn't load. YouTube may be refusing this server; try again in a minute."; msg.hidden = false; }
  });
  v.addEventListener("playing", () => { msg.hidden = true; });

  const rel = $("#related");
  fetch("/api/related/" + id, { credentials: "same-origin" }).then(r => r.text()).then(h => { rel.innerHTML = h; }).catch(() => { rel.textContent = "Couldn't load related videos."; });
}
