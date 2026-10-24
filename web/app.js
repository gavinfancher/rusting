/* app.js - shared helpers (fetch wrapper, toasts, formatters, SSE, nav) plus the logic of all three pages.
   The page is chosen by <body data-page="cluster|tournament|advisor">. `?mock=1` swaps the API for mock.js.
   All API data is inserted with textContent / text nodes, never innerHTML. */
'use strict';

const GRAFANA_URL = 'https://grafana.k8s.ad.gavinf.com';
const PARAMS = new URLSearchParams(location.search);
const MOCK = PARAMS.get('mock') === '1';
const SUITS = ['C', 'D', 'S', 'H'];
const SUIT_SYM = { C: '♣', D: '♦', S: '♠', H: '♥' };
const SUIT_NAME = { C: 'clubs', D: 'diamonds', S: 'spades', H: 'hearts' };
const RANKS = '23456789TJQKA';
const RANK_NAME = { T: '10', J: 'jack', Q: 'queen', K: 'king', A: 'ace' };

// ---------- DOM helpers ----------
const $ = (s, r = document) => r.querySelector(s);
/** Build an element. Props: class, on<event> handlers, booleans, else attributes. Kids are text or nodes. */
function el(tag, props = {}, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === 'class') e.className = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else if (v === true) e.setAttribute(k, '');
    else if (v !== false && v != null) e.setAttribute(k, v);
  }
  for (const kid of kids.flat()) if (kid != null && kid !== false) e.append(kid);
  return e;
}
const SVGNS = 'http://www.w3.org/2000/svg';
const svgEl = (tag, attrs) => { const e = document.createElementNS(SVGNS, tag); for (const k in attrs) e.setAttribute(k, attrs[k]); return e; };
/** Link href that keeps ?mock=1 across pages. */
const href = (path) => (MOCK ? path + (path.includes('?') ? '&' : '?') + 'mock=1' : path);

// ---------- formatters ----------
const fmtNum = (n, d = 0) => (n == null ? '-' : Number(n).toLocaleString(undefined, { maximumFractionDigits: d, minimumFractionDigits: d }));
function ago(iso) {
  const s = Math.max(0, Math.round((Date.now() - new Date(iso).getTime()) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  return `${Math.floor(s / 3600)}h ago`;
}
function dur(ms) {
  if (ms == null) return '-';
  const s = Math.round(ms / 1000);
  return s < 60 ? `${s}s` : s < 3600 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}
const badge = (status) => el('span', { class: `badge ${status}` }, status);

// ---------- toasts, fetch, polling, SSE ----------
function toast(msg) {
  const box = $('#toasts');
  if (!box) return;
  for (const t of box.children) if (t.textContent === msg) return; // de-duplicate polling errors
  const t = el('div', { class: 'toast' }, msg);
  box.append(t);
  setTimeout(() => t.remove(), 6000);
}
/** JSON request; errors (HTTP or network) raise a toast and reject. */
async function api(method, path, body) {
  try {
    if (MOCK) return await window.HEARTS_MOCK.handle(method, path, body);
    const r = await fetch(path, {
      method, headers: body ? { 'Content-Type': 'application/json' } : {},
      body: body ? JSON.stringify(body) : undefined,
    });
    let data = null;
    try { data = await r.json(); } catch { /* non-JSON error body */ }
    if (!r.ok) throw new Error((data && data.error) || `${r.status} ${r.statusText}`);
    return data;
  } catch (e) {
    toast(`${method} ${path}: ${e.message}`);
    throw e;
  }
}
/** Run fn now and then every ms after it finishes (so slow responses never pile up). */
function poll(fn, ms) {
  const run = async () => { try { await fn(); } catch { /* already toasted */ } setTimeout(run, ms); };
  run();
}
/** Subscribe to job events. Returns a close function. */
function subscribe(id, onProgress, onDone) {
  const path = `/api/jobs/${id}/events`;
  if (MOCK) return window.HEARTS_MOCK.sse(path, onProgress, onDone);
  const es = new EventSource(path);
  es.addEventListener('progress', (e) => onProgress(JSON.parse(e.data)));
  es.addEventListener('done', (e) => { onDone(JSON.parse(e.data)); es.close(); });
  es.onerror = () => { if (es.readyState === EventSource.CLOSED) toast('Event stream closed'); };
  return () => es.close();
}
function renderNav() {
  const nav = $('#nav'), page = document.body.dataset.page;
  const links = [['cluster', '/', 'Cluster'], ['tournament', '/tournament.html', 'Tournaments'], ['advisor', '/advisor.html', 'Advisor']];
  for (const [key, path, label] of links) {
    const a = el('a', { href: href(path === '/' ? 'index.html' : path.slice(1)) }, label);
    if (key === page) a.setAttribute('aria-current', 'page');
    nav.append(a);
  }
  if (MOCK) nav.append(el('span', { class: 'muted', style: 'padding:6px' }, '(mock data)'));
}

// ---------- Cluster page ----------
function initCluster() {
  $('#grafana').href = GRAFANA_URL;
  const nodeColor = new Map(); // stable colour per node name
  const colorOf = (n) => { if (!nodeColor.has(n)) nodeColor.set(n, `var(--n${nodeColor.size % 6})`); return nodeColor.get(n); };

  async function refresh() {
    const [workers, jobs] = await Promise.all([api('GET', '/api/workers'), api('GET', '/api/jobs?limit=20')]);
    workers.sort((a, b) => a.node_name.localeCompare(b.node_name) || a.pod_name.localeCompare(b.pod_name));
    const active = workers.filter((w) => w.active);
    $('#s-workers').textContent = active.length;
    $('#s-threads').textContent = active.reduce((s, w) => s + w.threads, 0);
    const running = jobs.find((j) => j.status === 'running');
    $('#s-hps').textContent = running && running.hands_per_sec != null ? fmtNum(running.hands_per_sec) : '-';
    const jobBox = $('#s-job');
    jobBox.replaceChildren(running ? el('a', { href: href(`tournament.html?id=${running.id}`) }, `#${running.id}`) : 'none');

    const perNode = new Map();
    for (const w of workers) {
      const n = perNode.get(w.node_name) || { pods: 0, active: 0, threads: 0 };
      n.pods++; if (w.active) { n.active++; n.threads += w.threads; }
      perNode.set(w.node_name, n);
    }
    $('#nodes').replaceChildren(...[...perNode].map(([name, n]) =>
      el('div', { class: 'stat', style: `border-left:6px solid ${colorOf(name)}` },
        el('div', { class: 'v' }, `${n.active}/${n.pods}`),
        el('div', { class: 'l' }, `${name}: active pods, ${n.threads} threads`))));

    $('#workers').replaceChildren(...workers.map((w) => el('tr', {},
      el('td', {}, w.pod_name),
      el('td', {}, el('span', { class: 'pill', style: `--pc:${colorOf(w.node_name)}` }, w.node_name)),
      el('td', { class: 'num' }, w.threads),
      el('td', {}, el('span', { class: `dot${w.active ? ' on' : ''}` }), w.active ? 'active' : 'stale'),
      el('td', { class: 'num' }, w.leased_units),
      el('td', {}, ago(w.last_seen)))));
  }
  poll(refresh, 2000);
}

// ---------- Tournament page ----------
const UNIT_STATES = ['pending', 'leased', 'done', 'failed', 'cancelled'];

function initTournament() {
  buildLaunchForm();
  poll(refreshJobList, 3000);
  const id = PARAMS.get('id');
  if (id) openDetail(id);
}

function buildLaunchForm() {
  const defaults = ['heuristic', 'heuristic', 'random', 'pimc'];
  const slots = $('#slots');
  defaults.forEach((d, i) => {
    const sel = el('select', { id: `f-s${i}` },
      ['random', 'heuristic', 'pimc'].map((k) => el('option', { value: k, selected: k === d }, k)));
    const samples = el('input', { type: 'number', id: `f-n${i}`, min: 1, value: 200, 'aria-label': `Slot ${i + 1} PIMC samples`, style: 'width:7em' });
    const sync = () => { samples.hidden = sel.value !== 'pimc'; };
    sel.addEventListener('change', sync); sync();
    slots.append(el('label', {}, `Seat ${i + 1}`, el('span', { style: 'display:flex;gap:4px' }, sel, samples)));
  });
  $('#launch').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const strategies = defaults.map((_, i) => {
      const k = $(`#f-s${i}`).value;
      return k === 'pimc' ? `pimc:${Math.max(1, parseInt($(`#f-n${i}`).value, 10) || 200)}` : k;
    });
    const body = { strategies, num_hands: parseInt($('#f-hands').value, 10), unit_size: parseInt($('#f-unit').value, 10),
      rules: { no_points_first_trick: $('#f-rule').checked } };
    if ($('#f-seed').value !== '') body.seed = parseInt($('#f-seed').value, 10);
    if ($('#f-name').value.trim()) body.name = $('#f-name').value.trim();
    try {
      const job = await api('POST', '/api/jobs', body);
      location.href = href(`tournament.html?id=${job.id}`);
    } catch { /* toasted */ }
  });
}

async function refreshJobList() {
  const jobs = await api('GET', '/api/jobs?limit=50');
  $('#jobs').replaceChildren(...jobs.map((j) => el('tr', {},
    el('td', {}, el('a', { href: href(`tournament.html?id=${j.id}`) }, `#${j.id}`)),
    el('td', {}, j.name),
    el('td', {}, badge(j.status)),
    el('td', {}, j.spec.strategies.join(', ')),
    el('td', { class: 'num' }, `${fmtNum(100 * j.progress.hands_done / j.num_hands, 0)}%`))));
}

function openDetail(id) {
  $('#detail').hidden = false;
  const history = []; // last 60 hands/sec samples for the sparkline
  let closeStream = null;
  const apply = (job, final) => {
    if (job.hands_per_sec != null && !final) { history.push(job.hands_per_sec); if (history.length > 60) history.shift(); }
    renderDetail(job, history);
  };
  $('#d-cancel').addEventListener('click', async () => {
    if (!confirm('Cancel this job?')) return;
    try { apply(await api('POST', `/api/jobs/${id}/cancel`), true); } catch { /* toasted */ }
  });
  api('GET', `/api/jobs/${id}`).then((j) => { apply(j); if (['running', 'pending'].includes(j.status)) {
    closeStream = subscribe(id, apply, (j2) => { apply(j2, true); closeStream = null; });
  } }).catch(() => {});
}

function renderDetail(job, history) {
  const p = job.progress, live = job.status === 'running' || job.status === 'pending';
  $('#d-title').textContent = `#${job.id} ${job.name}`;
  $('#d-status').replaceChildren(badge(job.status));
  $('#d-cancel').hidden = !live;
  $('#d-sub').textContent = `${job.spec.strategies.join(' / ')} - seed ${job.spec.seed} - ${fmtNum(job.num_hands)} hands in units of ${fmtNum(job.unit_size)}`;
  const pct = job.num_hands ? (100 * p.hands_done) / job.num_hands : 0;
  $('#d-progress').setAttribute('aria-valuenow', pct.toFixed(0));
  $('#d-progress-fill').style.width = `${pct}%`;
  $('#d-progress-text').textContent = `${fmtNum(p.hands_done)} / ${fmtNum(job.num_hands)} hands (${pct.toFixed(1)}%)`;

  const stack = $('#d-stack'), legend = $('#d-legend');
  stack.replaceChildren(...UNIT_STATES.map((s) => el('span', { style: `width:${p.units_total ? (100 * p[s]) / p.units_total : 0}%;background:var(--c-${s})` })));
  legend.replaceChildren(...UNIT_STATES.map((s) => el('span', {}, el('i', { style: `background:var(--c-${s})` }), `${s} ${p[s]}`)));
  $('#d-reclaims').replaceChildren(p.reclaims > 0
    ? el('span', { class: 'warn' }, `Reclaims: ${p.reclaims} - a worker died or was too slow, and its units were handed out again`)
    : el('span', { class: 'muted' }, 'Reclaims: 0'));
  $('#d-elapsed').textContent = dur(job.elapsed_ms);
  $('#d-hps').textContent = job.hands_per_sec != null ? `${fmtNum(job.hands_per_sec)} hands/s` : '-';
  renderSpark($('#d-spark'), history);

  const rows = job.standings.slice().sort((a, b) => a.avg_points - b.avg_points);
  const started = p.hands_done > 0;
  $('#d-standings').replaceChildren(...rows.map((s, i) => el('tr', { class: started && i === 0 ? 'best' : '' },
    el('td', {}, s.slot + 1), el('td', {}, s.strategy + (started && i === 0 ? ' (best)' : '')),
    el('td', { class: 'num' }, fmtNum(s.avg_points, 2)), el('td', { class: 'num' }, fmtNum(s.win_pct, 1)),
    el('td', { class: 'num' }, fmtNum(s.moons)), el('td', { class: 'num' }, fmtNum(s.queens)),
    el('td', { class: 'num' }, fmtNum(s.hands)))));
}

function renderSpark(svg, pts) {
  svg.replaceChildren();
  if (pts.length < 2) return;
  const max = Math.max(...pts) * 1.1 || 1, min = Math.min(...pts) * 0.9;
  const span = max - min || 1;
  const coords = pts.map((v, i) => `${((i / 59) * 240).toFixed(1)},${(48 - ((v - min) / span) * 46).toFixed(1)}`).join(' ');
  svg.append(svgEl('polyline', { points: coords }));
}

// ---------- Advisor page ----------
const cardName = (c) => `${RANK_NAME[c[0]] || c[0]} of ${SUIT_NAME[c[1]]}`;
const cardLabel = (c) => `${c[0] === 'T' ? '10' : c[0]}${SUIT_SYM[c[1]]}`;

function initAdvisor() {
  const sets = { hand: [], trick: [], played: [] };
  const limit = { hand: 13, trick: 3, played: 52 };
  let leaderTouched = false, brokenTouched = false;
  const mode = () => $('input[name=mode]:checked').value;
  const buttons = {};

  const deck = $('#deck');
  for (const s of SUITS) for (const r of RANKS) {
    const c = r + s;
    const b = el('button', { type: 'button', class: `card${s === 'D' || s === 'H' ? ' red' : ''}`, 'aria-pressed': 'false',
      'aria-label': cardName(c), onclick: () => toggle(c) }, cardLabel(c));
    buttons[c] = b; deck.append(b);
  }

  function where(c) { return Object.keys(sets).find((k) => sets[k].includes(c)); }
  function toggle(c) {
    const cur = where(c), m = mode();
    if (cur) sets[cur].splice(sets[cur].indexOf(c), 1);
    if (cur !== m) {
      if (sets[m].length >= limit[m]) { toast(`${m} is limited to ${limit[m]} cards`); if (cur) sets[cur].push(c); return; }
      sets[m].push(c);
    }
    refresh();
  }
  function refresh() {
    for (const c in buttons) {
      const w = where(c), b = buttons[c];
      b.classList.remove('s-hand', 's-trick', 's-played');
      if (w) b.classList.add(`s-${w}`);
      b.setAttribute('aria-pressed', String(!!w));
      b.replaceChildren(cardLabel(c), ...(w === 'trick' ? [el('small', {}, `#${sets.trick.indexOf(c) + 1}`)] : []));
    }
    const seat = +$('#a-seat').value;
    if (!leaderTouched) $('#a-leader').value = String((seat - sets.trick.length + 8) % 4);
    if (!brokenTouched) $('#a-broken').checked = [...sets.trick, ...sets.played].some((c) => c[1] === 'H');
    const list = (a) => (a.length ? a.map(cardLabel).join(' ') : 'none');
    $('#summary').textContent = `Hand (${sets.hand.length}/13): ${list(sets.hand)}. Trick: ${list(sets.trick)}. Played: ${sets.played.length} cards.`;
  }
  $('#a-leader').addEventListener('change', () => { leaderTouched = true; });
  $('#a-broken').addEventListener('change', () => { brokenTouched = true; });
  $('#a-seat').addEventListener('change', refresh);
  $('#a-samples').addEventListener('input', (e) => { $('#a-samples-v').textContent = fmtNum(e.target.value); });
  $('#clear').addEventListener('click', () => { for (const k in sets) sets[k].length = 0; leaderTouched = brokenTouched = false; refresh(); $('#result').hidden = true; });
  $('#random').addEventListener('click', () => {
    const all = Object.keys(buttons);
    for (let i = all.length - 1; i > 0; i--) { const j = Math.floor(Math.random() * (i + 1)); [all[i], all[j]] = [all[j], all[i]]; }
    sets.hand = all.slice(0, 13); sets.trick = []; sets.played = [];
    $('#a-seat').value = '0'; leaderTouched = brokenTouched = false;
    refresh(); // leader = seat 0 with an empty trick; the engine forces 2C when it is held
  });

  $('#advise').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    if (!sets.hand.length) { toast('Pick at least one card for your hand'); return; }
    const seat = +$('#a-seat').value;
    const body = { seat, hand: sets.hand, trick_leader: +$('#a-leader').value, trick: sets.trick, played: sets.played,
      hearts_broken: $('#a-broken').checked, tricks_played: Math.floor(sets.played.length / 4),
      samples: +$('#a-samples').value, deadline_ms: +$('#a-deadline').value || 2000 };
    const go = $('#a-go'); go.disabled = true; go.textContent = 'Thinking...';
    try { renderResult(await api('POST', '/api/advise', body)); } catch { /* toasted */ }
    go.disabled = false; go.textContent = 'Ask advisor';
  });
  refresh();
}

function renderResult(r) {
  $('#result').hidden = false;
  $('#r-alert').replaceChildren(r.partial
    ? el('div', { class: 'alert', role: 'alert' }, `Partial answer: only ${fmtNum(r.samples_achieved)} of ${fmtNum(r.samples_requested)} samples finished (${r.shards_failed} shard(s) failed or the deadline hit).`)
    : null);
  const max = Math.max(1, ...r.moves.map((m) => m.avg_points));
  const cells = [];
  for (const m of r.moves) {
    const best = m.card === r.best;
    cells.push(el('span', { class: best ? 'best-l' : '' }, cardLabel(m.card)),
      el('div', { class: 'track' }, el('div', { class: `fill${best ? ' best' : ''}`, style: `width:${(100 * m.avg_points) / max}%` })),
      el('span', { class: best ? 'best-l' : '' }, fmtNum(m.avg_points, 2) + (best ? ' best' : '')));
  }
  $('#r-bars').replaceChildren(...cells);
  $('#r-stats').replaceChildren(
    el('span', {}, `Samples: ${fmtNum(r.samples_achieved)} / ${fmtNum(r.samples_requested)}`),
    el('span', {}, `Shards: ${r.shards} (${r.shards_failed} failed)`),
    el('span', {}, `Latency: ${fmtNum(r.latency_ms)} ms`));
}

// ---------- boot ----------
renderNav();
const pages = { cluster: initCluster, tournament: initTournament, advisor: initAdvisor };
pages[document.body.dataset.page]();
