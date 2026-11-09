// YourTube web: card menus, actions, background refreshes, the player, and the motion. No frameworks.
"use strict";

const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => Array.from(el.querySelectorAll(s));
const incognito = () => document.body.classList.contains("incognito");
const root = document.documentElement;
const reduced = matchMedia("(prefers-reduced-motion: reduce)");
const ease = "cubic-bezier(.2, .8, .2, 1)";

root.classList.add("js");
const navType = (performance.getEntriesByType?.("navigation")[0] || {}).type;
// Coming back to a page (the Back button, a swipe): its cards don't wave in again.
if (navType === "back_forward") root.classList.add("back-nav");
// iPhones only show :active (the press) while something listens for touches.
addEventListener("touchstart", () => {}, { passive: true });

// Session storage can be missing or full (private modes): everything here works without it.
const store = {
  get(k) { try { return JSON.parse(sessionStorage.getItem(k)); } catch (_) { return null; } },
  set(k, v) { try { sessionStorage.setItem(k, JSON.stringify(v)); return true; } catch (_) { return false; } },
};
const here = () => location.pathname + location.search;

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

// Cards leave by fading and shrinking, and the ones after them slide up into the gap.
async function removeCards(cards) {
  cards = cards.filter(c => c.isConnected);
  if (!cards.length) return;
  if (reduced.matches || !Element.prototype.animate) return cards.forEach(c => c.remove());
  await Promise.all(cards.map(c => c.animate(
    [{ opacity: 1, transform: "none" }, { opacity: 0, transform: "scale(.94)" }],
    { duration: 220, easing: ease, fill: "forwards" },
  ).finished.catch(() => {})));
  const near = c => { const r = c.getBoundingClientRect(); return r.bottom > -200 && r.top < innerHeight + 200; };
  const rest = $$(".card").filter(c => !cards.includes(c) && near(c));
  const before = new Map(rest.map(c => [c, c.getBoundingClientRect()]));
  cards.forEach(c => c.remove());
  for (const [c, a] of before) {
    const b = c.getBoundingClientRect();
    const dx = a.left - b.left, dy = a.top - b.top;
    if (dx || dy) c.animate([{ transform: `translate(${dx}px, ${dy}px)` }, { transform: "none" }], { duration: 360, easing: ease });
  }
}

async function act(name, v, el) {
  if (name === "copy") return copyLink(v.id);
  const j = await post("/api/action", { act: name, ...v });
  if (j.error) return toast(j.error);
  toast(j.msg);
  const card = el && el.closest(".card");
  if (j.hide && card) removeCards([card]);
  if (j.hide_channel) removeCards($$(".card").filter(c => c.dataset.cid === j.hide_channel));
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

// ---------------------------------------------------------------------------------------------
// Motion between pages. Cross-document view transitions (style.css) carry the opened thumbnail
// into the video page and back, and the tab pill across; everything below also works without them.

// Pictures fade in over the shimmer once they've arrived.
function watchPictures(scope = document) {
  for (const img of $$(".thumb img", scope)) {
    const done = () => img.parentElement.classList.add("ok");
    if (img.complete && img.naturalWidth) done();
    else { img.addEventListener("load", done, { once: true }); img.addEventListener("error", done, { once: true }); }
  }
}
watchPictures();

// One element at a time carries the "hero" name (two would cancel the transition).
let named = null;
function nameHero(el) {
  if (named && named !== el) named.style.viewTransitionName = "";
  named = el;
  if (el) el.style.viewTransitionName = "hero";
}

// Per page: where it was scrolled to and which video was opened from it, for coming back.
// Kept in this tab's session storage only, and not at all in incognito (the keys are addresses,
// searches included).
const SPOTS = "yt-spots";
function remember(extra = {}) {
  if (incognito()) return;
  const spots = store.get(SPOTS) || {};
  spots[here()] = { ...(spots[here()] || {}), y: Math.round(scrollY), ...extra };
  const keys = Object.keys(spots);
  for (const k of keys.slice(0, Math.max(0, keys.length - 30))) delete spots[k];
  store.set(SPOTS, spots);
}

// Where the browser announces a page's first frame (pagereveal), this script puts the scroll
// back itself, before that frame, so the thumbnail flies back to where its card really is.
// Elsewhere (or without session storage) the browser does it as usual.
const ownScroll = "onpagereveal" in window && !incognito() && store.set("yt-ok", true);
if (ownScroll) history.scrollRestoration = "manual";

function moveTabPill(a) {
  const nav = a.closest(".tabs");
  const i = $$("a", nav).indexOf(a);
  if (i < 0) return;
  nav.dataset.tab = String(i);
  $$("a", nav).forEach(x => (x === a ? x.setAttribute("aria-current", "page") : x.removeAttribute("aria-current")));
}

// Runs after the handlers above, so their preventDefault() counts.
document.addEventListener("click", e => {
  if (e.defaultPrevented || e.button || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
  const back = e.target.closest("[data-back]");
  if (back) {
    // Back to the page this one was opened from; straight here from elsewhere, back to For You.
    if (history.length > 1 && store.get("yt-inside")) { e.preventDefault(); history.back(); }
    return;
  }
  const a = e.target.closest("a[href]");
  if (!a || a.origin !== location.origin || a.target) return;
  store.set("yt-inside", true);
  const card = a.closest(".card");
  if (card && a.pathname === "/watch") {
    // This card's picture is the one that flies; the player of a video page we're leaving can't also.
    const player = $(".player");
    if (player) player.style.viewTransitionName = "none";
    nameHero($(".thumb", card));
    remember({ id: card.dataset.id });
  } else {
    nameHero(null);
    remember({ id: null });
  }
  if (a.closest(".tabs")) moveTabPill(a);
});

// (Not while a search is on its way: the page was scrolled up for the stand-ins.)
addEventListener("pagehide", () => { if (!keptMain) remember(); });

// Coming back (or reloading): scroll to where we were, and on the way back let the opened
// video's picture fly into its card.
function cameBack(e, flight = true) {
  if (flight && e.viewTransition) e.viewTransition.types?.add("back");
  const spot = (store.get(SPOTS) || {})[here()];
  if (!spot) return;
  if (ownScroll && !e.persisted && spot.y) scrollTo(0, spot.y);
  // Only into a card that's on screen: the page stays where you left it (History, for one, has
  // moved the video you just watched to the top).
  const card = flight && spot.id && $$(".card").find(c => c.dataset.id === spot.id);
  const r = card && $(".thumb", card).getBoundingClientRect();
  const vt = e.viewTransition;
  if (!vt || !r || r.bottom < 0 || r.top > innerHeight) return nameHero(null);
  const img = $(".thumb img", card);
  if (img) { img.loading = "eager"; img.parentElement.classList.add("ok"); }
  nameHero($(".thumb", card));
  vt.finished.finally(() => nameHero(null));
}
let restored = false;
addEventListener("pageshow", e => {
  if (!e.persisted) return;
  restored = true;
  root.classList.add("back-nav");
  // A search that was on its way when we left: put the page back as it was.
  if (keptMain) {
    $("main").replaceChildren(...keptMain);
    keptMain = null;
    const spot = (store.get(SPOTS) || {})[here()];
    if (spot && spot.y) scrollTo(0, spot.y);
  }
  $$(".search.busy").forEach(f => f.classList.remove("busy"));
  if (!ownScroll) cameBack({ persisted: true });
});
if (ownScroll) {
  addEventListener("pagereveal", e => {
    if (restored || navType === "back_forward") cameBack({ persisted: restored, viewTransition: e.viewTransition });
    else if (navType === "reload") cameBack({}, false);
    restored = false;
  });
}

// A search takes a few seconds: show where the results will land while it's on its way.
let keptMain = null;
document.addEventListener("submit", e => {
  const f = e.target;
  if (!f.matches(".search") || !f.q.value.trim() || e.defaultPrevented) return;
  store.set("yt-inside", true);
  remember();
  f.classList.add("busy");
  const main = $("main");
  if (!main || keptMain) return;
  keptMain = Array.from(main.childNodes);
  const card = '<div class="card sk"><span class="thumb"></span><div class="meta"><span class="bone"></span><span class="bone mid"></span><span class="bone short"></span></div></div>';
  main.innerHTML = '<div class="head sk" aria-hidden="true"><div><span class="bone"></span><span class="bone mid"></span></div></div>'
    + '<p class="sr-only" role="status">Searching…</p><section class="grid" aria-hidden="true">' + card.repeat(4) + "</section>";
  scrollTo(0, 0);
});

// ---------------------------------------------------------------------------------------------
// The watch page.
const watch = $(".watch");
if (watch) {
  const v = $("#video");
  const player = $(".player");
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

  // Once it plays, the video shows whole again instead of cropped like the poster. (Not on the
  // resume seek: an iPhone keeps showing the poster until it plays.)
  for (const ev of ["play", "playing"]) v.addEventListener(ev, () => player.classList.add("started"));

  // The Play pill under the picture mirrors the player.
  const syncPlay = () => {
    const on = !v.paused && !v.ended;
    for (const b of $$("[data-play]")) {
      b.classList.toggle("on", on);
      $("span", b).textContent = on ? "Pause" : "Play";
    }
  };
  for (const ev of ["play", "pause", "ended", "playing"]) v.addEventListener(ev, syncPlay);

  // Chapters: tap one to jump there; the one playing is tinted.
  let chapters = [];
  const findChapters = () => { chapters = $$(".chap").map(b => [parseFloat(b.dataset.t) || 0, b]); };
  let current = null;
  v.addEventListener("timeupdate", () => {
    let now = null;
    for (const [t, b] of chapters) if (v.currentTime + 0.25 >= t) now = b;
    if (now !== current) { current?.classList.remove("now"); now?.classList.add("now"); current = now; }
  });

  // The description shows five lines; "more" opens the rest.
  const fitAbout = () => {
    for (const p of $$(".desc-text")) {
      const more = p.parentElement.querySelector(".more-text");
      if (more) more.hidden = p.scrollHeight <= p.clientHeight + 2;
    }
  };

  document.addEventListener("click", e => {
    const play = e.target.closest("[data-play]");
    if (play) {
      if (v.paused || v.ended) v.play().catch(() => {}); else v.pause();
      return;
    }
    const chap = e.target.closest(".chap");
    if (chap) {
      v.currentTime = parseFloat(chap.dataset.t) || 0;
      player.classList.add("started");
      v.play().catch(() => {});
      return;
    }
    const more = e.target.closest(".more-text");
    if (more) {
      const p = more.parentElement.querySelector(".desc-text");
      const open = p.classList.toggle("open");
      more.textContent = open ? "less" : "more";
      more.setAttribute("aria-expanded", String(open));
    }
  });

  const infoReady = () => { syncPlay(); findChapters(); fitAbout(); };
  infoReady();

  // No answer: keep what the database knew (the facts line) and drop the shining bars.
  const settle = () => {
    const a = $(".about.loading");
    if (!a) return;
    $$(".bone", a).forEach(b => b.remove());
    a.classList.remove("loading");
    a.removeAttribute("aria-busy");
    if (!a.children.length) a.remove();
  };

  // Title, channel and description from yt-dlp (it is asked anyway to find the stream).
  fetch("/api/info/" + id, { credentials: "same-origin" }).then(r => r.json()).then(j => {
    if (!j.ok) {
      if (j.error) { msg.textContent = "Can't play this video: " + j.error; msg.hidden = false; }
      settle();
      return;
    }
    if (j.html) { $(".v-info").innerHTML = j.html; infoReady(); }
    Object.assign(watch.dataset, { title: j.title || "", channel: j.channel || "", cid: j.channel_id || "", len: j.length ? Math.round(j.length) : "" });
    if (j.title) document.title = j.title + " · YourTube";
  }).catch(settle);

  // With <source> children the browser tries each in turn (HLS where it plays, else the MP4);
  // only the last one failing means nothing will play.
  const failed = () => {
    if (msg.hidden) { msg.textContent = "The video didn't load. YouTube may be refusing this server; try again in a minute."; msg.hidden = false; }
  };
  v.addEventListener("error", failed);
  const last = v.querySelector("source:last-of-type");
  if (last) last.addEventListener("error", failed);
  v.addEventListener("playing", () => { msg.hidden = true; });

  // Related videos rise in when they land, in place of the shining stand-ins.
  const rel = $("#related");
  fetch("/api/related/" + id, { credentials: "same-origin" }).then(r => r.text()).then(h => {
    rel.innerHTML = h;
    rel.removeAttribute("aria-busy");
    watchPictures(rel);
  }).catch(() => { rel.textContent = "Couldn't load related videos."; rel.removeAttribute("aria-busy"); });
}
