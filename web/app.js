/* app.js - shared helpers (fetch wrapper, toasts, formatters, SSE, nav) plus the logic of all three pages.
   The page is chosen by <body data-page="cluster|tournament|training|advisor">. `?mock=1` swaps the API for mock.js.
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
  const links = [['cluster', '/', 'Cluster'], ['tournament', '/tournament.html', 'Tournaments'], ['training', '/training.html', 'Training'], ['advisor', '/advisor.html', 'Advisor']];
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
      ['random', 'heuristic', 'pimc', 'custom'].map((k) => el('option', { value: k, selected: k === d }, k === 'custom' ? 'custom...' : k)));
    const samples = el('input', { type: 'number', id: `f-n${i}`, min: 1, value: 200, 'aria-label': `Slot ${i + 1} PIMC samples`, style: 'width:7em' });
    // Free-text strategy: e.g. champion:3 (best weights of training run 3) or linear:w0,...,w23.
    const custom = el('input', { type: 'text', id: `f-c${i}`, placeholder: 'champion:3', 'aria-label': `Slot ${i + 1} custom strategy`, style: 'width:9em' });
    const sync = () => { samples.hidden = sel.value !== 'pimc'; custom.hidden = sel.value !== 'custom'; };
    sel.addEventListener('change', sync); sync();
    slots.append(el('label', {}, `Seat ${i + 1}`, el('span', { style: 'display:flex;gap:4px' }, sel, samples, custom)));
  });
  $('#launch').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const strategies = defaults.map((_, i) => {
      const k = $(`#f-s${i}`).value;
      if (k === 'custom') return $(`#f-c${i}`).value.trim();
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

// ---------- Training page ----------
// Plain-English meaning of each weight (names are hearts_core::types::FEATURE_NAMES).
const FEATURE_HINTS = {
  'play.leading': 'Playing any card when we lead the trick',
  'play.rank': 'Preference for high cards (negative = play low cards)',
  'play.is_heart': 'The card is a heart',
  'play.is_queen_spades': 'The card is the queen of spades (13 points)',
  'play.high_spade_queen_unseen': 'King/ace of spades while the queen is still out there',
  'play.wins_now': 'The card would currently win the trick',
  'play.wins_x_points_on_table': 'Winning a trick that already holds penalty points',
  'play.wins_x_last_to_play': 'Winning when we play last (a sure win)',
  'play.wins_x_my_points': 'Winning when we already hold points (moon shooting)',
  'play.dump_x_points': 'Void in the led suit: dumping a penalty card',
  'play.dump_x_rank': 'Void in the led suit: dumping a high card',
  'play.makes_void': 'The play leaves us with no cards in that suit',
  'play.suit_len_after': 'How many cards of that suit we would still hold',
  'play.higher_unseen': 'Unseen cards of the suit that beat this one',
  'play.lower_unseen': 'Unseen cards of the suit below this one',
  'play.lead_low_spade_fishing': 'Leading a low spade to flush out the queen',
  'pass.rank': 'Passing high cards',
  'pass.heart_x_rank': 'Passing high hearts',
  'pass.is_queen_spades': 'Passing the queen of spades',
  'pass.is_high_spade': 'Passing the king or ace of spades',
  'pass.spades_guarded_x_high': 'Passing a high spade when we hold 3+ low spades to guard',
  'pass.short_side_suit': 'Passing from a short club or diamond suit (to void it)',
  'pass.is_low': 'Passing low cards (rank 5 or below)',
  'pass.side_suit_x_rank': 'Passing high clubs or diamonds',
};
const TRAIN_TERMINAL = ['done', 'failed', 'cancelled'];

function initTraining() {
  buildTrainForm();
  poll(refreshRunList, 3000);
  const id = PARAMS.get('id');
  if (id) openRun(id);
}

function buildTrainForm() {
  const box = $('#f-opps');
  for (let i = 0; i < 3; i++) {
    box.append(el('label', {}, `Opponent ${i + 1}`,
      el('input', { type: 'text', id: `f-o${i}`, value: 'heuristic', style: 'width:11em', 'aria-label': `Opponent ${i + 1} strategy` })));
  }
  box.append(el('span', { class: 'muted', style: 'align-self:end;padding-bottom:8px' },
    'Strategies: random, heuristic, pimc:N, champion:RUN, linear:w0,...'));
  $('#launch').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const num = (id) => ($(id).value === '' ? undefined : Number($(id).value));
    const body = {
      population: num('#f-pop'), generations: num('#f-gens'), hands_per_candidate: num('#f-hands'), init: $('#f-init').value,
      opponents: [0, 1, 2].map((i) => $(`#f-o${i}`).value.trim()), rules: { no_points_first_trick: $('#f-rule').checked },
      seed: num('#f-seed'), elites: num('#f-elites'), unit_size: num('#f-unit'), sigma0: num('#f-sigma0'),
      smoothing: num('#f-smooth'), min_sigma: num('#f-minsig'), validation_hands: num('#f-vhands'),
    };
    if ($('#f-name').value.trim()) body.name = $('#f-name').value.trim();
    for (const k of Object.keys(body)) if (body[k] === undefined) delete body[k]; // let the server apply its defaults
    try {
      const run = await api('POST', '/api/training', body);
      location.href = href(`training.html?id=${run.id}`);
    } catch { /* toasted */ }
  });
}

async function refreshRunList() {
  const runs = await api('GET', '/api/training?limit=50');
  $('#runs').replaceChildren(...runs.map((r) => el('tr', {},
    el('td', {}, el('a', { href: href(`training.html?id=${r.id}`) }, `#${r.id}`)),
    el('td', {}, r.name || ''),
    el('td', {}, badge(r.status)),
    el('td', { class: 'num' }, `${r.generation}/${r.generations_total}`),
    el('td', { class: 'num' }, r.champion && r.champion.fitness != null ? fmtNum(r.champion.fitness, 2) : '-'),
    el('td', {}, r.validation ? `${fmtNum(r.validation.champion_fitness, 2)} vs ${fmtNum(r.validation.baseline_fitness, 2)} (heuristic)` : '-'),
    el('td', {}, ago(r.created_at)))));
}

function openRun(id) {
  $('#detail').hidden = false;
  let prev = null; // previous poll, to estimate hands/sec from the growth of hands_done
  $('#d-cancel').addEventListener('click', async () => {
    if (!confirm('Cancel this training run and its jobs?')) return;
    try { renderRun(await api('POST', `/api/training/${id}/cancel`), null); } catch { /* toasted */ }
  });
  $('#d-copy').addEventListener('click', async () => {
    const text = $('#d-champion-code').textContent, b = $('#d-copy');
    try { await navigator.clipboard.writeText(text); b.textContent = 'Copied'; }
    catch { const r = document.createRange(); r.selectNodeContents($('#d-champion-code')); getSelection().removeAllRanges(); getSelection().addRange(r); b.textContent = 'Selected - press Ctrl+C'; }
    setTimeout(() => { b.textContent = 'Copy'; }, 2000);
  });
  const tick = async () => {
    let run = null;
    try {
      run = await api('GET', `/api/training/${id}`);
      let hps = null;
      const cur = run.current, now = Date.now();
      if (cur && prev && prev.gen === cur.generation && cur.hands_done > prev.hands) {
        hps = (cur.hands_done - prev.hands) / ((now - prev.at) / 1000);
      }
      prev = cur ? { gen: cur.generation, hands: cur.hands_done, at: now } : null;
      renderRun(run, hps);
    } catch { /* toasted; retry */ }
    if (!run || !TRAIN_TERMINAL.includes(run.status)) setTimeout(tick, 2000);
  };
  tick();
}

function renderRun(run, hps) {
  const live = !TRAIN_TERMINAL.includes(run.status), cfg = run.config, cur = run.current;
  $('#d-title').textContent = `#${run.id} ${run.name || ''}`;
  $('#d-status').replaceChildren(badge(run.status));
  $('#d-cancel').hidden = !live;
  $('#d-sub').textContent = `population ${cfg.population}, ${fmtNum(cfg.hands_per_candidate)} hands per candidate, start ${cfg.init}, `
    + `opponents ${cfg.opponents.join(' / ')}, seed ${run.seed}`;
  $('#d-error').replaceChildren(...(run.last_error ? [el('div', { class: 'alert', role: 'alert' }, `Trainer error: ${run.last_error}`)] : []));

  let pct = 0, text = '';
  if (run.status === 'validating') {
    $('#d-gen').textContent = `All ${run.generations_total} generations done - validating the champion against the heuristic`;
  } else if (run.status === 'done') {
    $('#d-gen').textContent = `Finished ${run.generations_total} generations`; pct = 100;
  } else {
    $('#d-gen').textContent = `Generation ${Math.min(run.generation + (cur ? 1 : 0), run.generations_total)}/${run.generations_total}`
      + (live ? '' : ` (stopped after ${run.generation} complete)`);
  }
  if (cur) {
    pct = cur.hands_total ? (100 * cur.hands_done) / cur.hands_total : 0;
    text = `${cur.candidates_done}/${cur.candidates_total} candidates scored - ${fmtNum(cur.hands_done)} / ${fmtNum(cur.hands_total)} hands`
      + (hps != null ? ` - ${fmtNum(hps)} hands/s` : '');
  }
  $('#d-progress').setAttribute('aria-valuenow', pct.toFixed(0));
  $('#d-progress-fill').style.width = `${pct}%`;
  $('#d-progress-text').textContent = text;

  const baseline = run.validation && run.validation.baseline_fitness != null ? run.validation.baseline_fitness : 6.5;
  renderCurve($('#d-chart'), $('#d-chart-legend'), run.generations || [], run.generations_total, baseline, !!run.validation);

  $('#d-champion-panel').hidden = !run.champion;
  $('#d-weights-panel').hidden = !run.champion;
  if (run.champion) {
    const c = run.champion;
    $('#d-champion-fit').textContent = c.fitness != null
      ? `Measured ${fmtNum(c.fitness, 2)} points per hand in its last generation (heuristic baseline ${fmtNum(baseline, 2)}).` : '';
    $('#d-champion-code').textContent = c.strategy;
    $('#d-alias').textContent = `champion:${run.id}`;
    const v = run.validation;
    $('#d-validation').replaceChildren(v
      ? el('b', {}, `Validation: champion ${fmtNum(v.champion_fitness, 2)} vs heuristic ${fmtNum(v.baseline_fitness, 2)} on the same ${fmtNum(v.hands)} hands`)
      : el('span', { class: 'muted' }, 'Validation pending: runs after the last generation.'));
    const max = Math.max(1, ...c.weights.map((w) => Math.abs(w.weight)));
    $('#d-weights').replaceChildren(...c.weights.map((w) => {
      const frac = (50 * Math.abs(w.weight)) / max;
      const bar = el('i', { class: w.weight < 0 ? 'neg' : '', style: `width:${frac}%;${w.weight < 0 ? 'right:50%' : 'left:50%'}` });
      return el('tr', {},
        el('td', {}, w.feature), el('td', { class: 'hint' }, FEATURE_HINTS[w.feature] || ''),
        el('td', { class: 'num' }, (w.weight > 0 ? '+' : '') + w.weight),
        el('td', {}, el('div', { class: 'wbar', 'aria-hidden': 'true' }, bar)));
    }));
  }
}

/** Learning curve: mean_fitness and best_fitness per generation plus a dashed heuristic-baseline line. */
function renderCurve(svg, legend, gens, total, baseline, validated) {
  const W = 640, H = 300, L = 58, R = 16, T = 12, B = 40;
  svg.replaceChildren();
  const vals = gens.flatMap((g) => [g.mean_fitness, g.best_fitness]).filter((v) => v != null);
  vals.push(baseline);
  let lo = Math.min(...vals), hi = Math.max(...vals);
  const pad = Math.max(0.05, (hi - lo) * 0.1); lo -= pad; hi += pad;
  const x = (g) => L + (total > 1 ? (g / (total - 1)) * (W - L - R) : (W - L - R) / 2);
  const y = (v) => T + (1 - (v - lo) / (hi - lo)) * (H - T - B);
  const t = (txt, attrs) => { const e = svgEl('text', attrs); e.textContent = txt; return e; };

  for (let i = 0; i <= 4; i++) {            // horizontal grid + y tick labels
    const v = lo + ((hi - lo) * i) / 4;
    svg.append(svgEl('line', { class: 'grid', x1: L, x2: W - R, y1: y(v), y2: y(v) }),
      t(v.toFixed(2), { x: L - 6, y: y(v) + 4, 'text-anchor': 'end' }));
  }
  const step = Math.max(1, Math.ceil(total / 10));
  for (let g = 0; g < total; g += step) {   // x ticks (generations numbered from 1)
    svg.append(svgEl('line', { class: 'axis', x1: x(g), x2: x(g), y1: H - B, y2: H - B + 4 }),
      t(String(g + 1), { x: x(g), y: H - B + 16, 'text-anchor': 'middle' }));
  }
  svg.append(svgEl('line', { class: 'axis', x1: L, x2: L, y1: T, y2: H - B }),
    svgEl('line', { class: 'axis', x1: L, x2: W - R, y1: H - B, y2: H - B }),
    t('generation', { x: (L + W - R) / 2, y: H - 6, 'text-anchor': 'middle' }),
    t('avg points / hand (lower is better)', { x: 14, y: (T + H - B) / 2, 'text-anchor': 'middle', transform: `rotate(-90 14 ${(T + H - B) / 2})` }));

  svg.append(svgEl('line', { class: 'l-base', x1: L, x2: W - R, y1: y(baseline), y2: y(baseline) }),
    t(`heuristic ${baseline.toFixed(2)}`, { x: W - R - 4, y: y(baseline) - 5, 'text-anchor': 'end' }));
  const series = (key, cls, ptCls) => {
    const pts = gens.filter((g) => g[key] != null);
    if (!pts.length) return;
    svg.append(svgEl('polyline', { class: cls, points: pts.map((g) => `${x(g.generation).toFixed(1)},${y(g[key]).toFixed(1)}`).join(' ') }));
    for (const g of pts) svg.append(svgEl('circle', { class: ptCls, cx: x(g.generation), cy: y(g[key]), r: 3 }));
  };
  series('mean_fitness', 'l-mean', 'pt-mean');
  series('best_fitness', 'l-best', 'pt-best');

  const last = gens[gens.length - 1];
  svg.setAttribute('aria-label', gens.length
    ? `Learning curve over ${gens.length} generations: mean fitness ${last.mean_fitness != null ? last.mean_fitness.toFixed(2) : 'n/a'}, `
      + `best ${last.best_fitness != null ? last.best_fitness.toFixed(2) : 'n/a'}, heuristic baseline ${baseline.toFixed(2)}. Lower is better.`
    : 'Learning curve: no generation has finished yet.');
  legend.replaceChildren(
    el('span', {}, el('i', { class: 'sw' }), 'mean of the belief (candidate 0)'),
    el('span', {}, el('i', { class: 'sw best' }), 'best candidate'),
    el('span', {}, el('i', { class: 'sw base' }), validated ? 'heuristic (validation, same hands)' : 'heuristic baseline (about 6.5)'),
    ...(gens.length ? [] : [el('span', { class: 'muted' }, 'Waiting for the first generation to finish...')]));
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
const pages = { cluster: initCluster, tournament: initTournament, training: initTraining, advisor: initAdvisor };
pages[document.body.dataset.page]();
