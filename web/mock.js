/* mock.js - fixtures and a fake backend for ?mock=1. Shapes follow docs/CONTRACTS.md exactly.
   Exposes window.HEARTS_MOCK = { handle(method, path, body), sse(path, onProgress, onDone) }. */
(function () {
  const t0 = Date.now();
  const iso = (ms) => new Date(ms).toISOString();
  const HPS = 5500; // simulated cluster throughput for the running job

  // Mutable in-memory jobs. The running job starts at page load so progress visibly advances.
  const jobs = [
    { id: 1, name: "heuristic vs random", strategies: ["heuristic", "heuristic", "random", "pimc:200"], num_hands: 100000,
      unit_size: 2000, seed: 42, status: "running", created: t0 - 2000, started: t0 - 1000, finished: null, fixed: null },
    { id: 2, name: "baseline sanity", strategies: ["random", "random", "random", "random"], num_hands: 20000,
      unit_size: 2000, seed: 7, status: "done", created: t0 - 3600e3, started: t0 - 3599e3, finished: t0 - 3590e3, fixed: 1 },
    { id: 3, name: "pimc depth test", strategies: ["pimc:50", "pimc:500", "heuristic", "heuristic"], num_hands: 50000,
      unit_size: 5000, seed: 99, status: "cancelled", created: t0 - 7200e3, started: t0 - 7199e3, finished: t0 - 7150e3, fixed: 0.4 },
  ];
  // Mean points per hand by strategy family: lower is better.
  const AVG = { random: 7.4, heuristic: 5.6, pimc: 4.9, linear: 5.4 };

  function frac(j) {
    if (j.fixed != null) return j.fixed;
    return Math.min(1, ((Date.now() - j.started) / 1000) * HPS / j.num_hands);
  }
  function view(j) {
    const f = frac(j);
    if (j.fixed == null && f >= 1 && j.status === "running") { j.status = "done"; j.finished = Date.now(); }
    const total = Math.ceil(j.num_hands / j.unit_size);
    const done = Math.floor(f * total);
    const terminal = j.status !== "running" && j.status !== "pending";
    const leased = terminal ? 0 : Math.min(total - done, 6);
    const cancelled = j.status === "cancelled" ? total - done : 0;
    const pending = terminal ? 0 : total - done - leased;
    const handsDone = Math.min(j.num_hands, done * j.unit_size);
    const elapsed = (j.finished || Date.now()) - j.started;
    const standings = j.strategies.map((s, slot) => {
      const k = s.split(":")[0], avg = AVG[k] + (slot % 2) * 0.1;
      const points = Math.round(avg * handsDone);
      const wins = Math.round(handsDone * (k === "pimc" ? 0.35 : k === "heuristic" ? 0.27 : 0.11));
      return { slot, strategy: s, hands: handsDone, points,
        avg_points: handsDone ? +(points / handsDone).toFixed(2) : 0,
        moons: Math.round(handsDone * 0.002), queens: Math.round(handsDone * 0.25), wins,
        win_pct: handsDone ? +(100 * wins / handsDone).toFixed(1) : 0 };
    });
    return {
      id: j.id, name: j.name, status: j.status,
      spec: { seed: j.seed, strategies: j.strategies, rules: { no_points_first_trick: true } },
      num_hands: j.num_hands, unit_size: j.unit_size,
      created_at: iso(j.created), started_at: iso(j.started), finished_at: j.finished ? iso(j.finished) : null,
      progress: { units_total: total, pending, leased, done, failed: 0, cancelled, hands_done: handsDone,
        reclaims: j.id === 1 && done > total * 0.3 ? 2 : 0 },
      standings, elapsed_ms: elapsed,
      hands_per_sec: handsDone ? +(j.status === "running" ? HPS * (0.9 + Math.random() * 0.2) : handsDone / (elapsed / 1000)).toFixed(1) : null,
    };
  }


  // ---------- training fixtures ----------
  const FEATURES = ["play.leading", "play.rank", "play.is_heart", "play.is_queen_spades", "play.high_spade_queen_unseen",
    "play.wins_now", "play.wins_x_points_on_table", "play.wins_x_last_to_play", "play.wins_x_my_points", "play.dump_x_points",
    "play.dump_x_rank", "play.makes_void", "play.suit_len_after", "play.higher_unseen", "play.lower_unseen",
    "play.lead_low_spade_fishing", "pass.rank", "pass.heart_x_rank", "pass.is_queen_spades", "pass.is_high_spade",
    "pass.spades_guarded_x_high", "pass.short_side_suit", "pass.is_low", "pass.side_suit_x_rank"];
  // A plausible "learned" weight vector (the final champion of a long run).
  const FINAL_W = [-12, -3, -30, -90, -40, -25, -6, 18, -10, 14, 2, 9, -2, -4, 3, 7, 5, 4, 80, 35, -20, 6, -3, 2];
  const TRAIN_STEP_MS = 5000; // the live mock run finishes one generation every 5 s
  const runs = [
    { id: 1, name: "first run", gens: 30, pop: 24, hpc: 40000, init: "heuristic", seed: 1, created: t0 - 86400e3, done: true },
    { id: 2, name: "from scratch (zero init)", gens: 30, pop: 24, hpc: 40000, init: "zero", seed: 2, created: t0 - 1000, done: false },
  ];
  // Learning curve: exponential approach to a floor, with deterministic wiggle.
  function curveAt(r, g) {
    const start = r.init === "zero" ? 8.6 : 6.5, floor = r.init === "zero" ? 5.95 : 5.9, tau = r.init === "zero" ? 7 : 9;
    const mean = floor + (start - floor) * Math.exp(-g / tau) + 0.05 * Math.sin(g * 1.7);
    const best = mean - 0.16 - 0.04 * Math.abs(Math.cos(g * 2.3));
    return { mean, best, elite: mean - 0.1, pop: mean + 0.35 };
  }
  function trainGen(r) {
    if (r.frozen != null) return r.frozen;
    if (r.done) return r.gens;
    return Math.min(r.gens, Math.floor((Date.now() - r.created) / TRAIN_STEP_MS));
  }
  function trainView(r, withGens) {
    const g = trainGen(r), allDone = g >= r.gens;
    let status = r.status || "running";
    if (!r.status) {
      if (r.done) status = "done";
      else if (allDone) status = Date.now() - r.created > r.gens * TRAIN_STEP_MS + 8000 ? "done" : "validating";
    }
    if (!r.status && status === "done" && !r.done) r.finishedAt = r.finishedAt || Date.now();
    const gens = [];
    for (let i = 0; i < g; i++) {
      const c = curveAt(r, i), at = r.created + i * TRAIN_STEP_MS;
      gens.push({ generation: i, seed: 1000 + i * 7919, mean_fitness: +c.mean.toFixed(3), best_fitness: +c.best.toFixed(3),
        elite_mean_fitness: +c.elite.toFixed(3), population_mean_fitness: +c.pop.toFixed(3),
        sigma_mean: +(2 + 18 * Math.exp(-i / 12)).toFixed(2), started_at: iso(at), finished_at: iso(at + TRAIN_STEP_MS),
        elapsed_ms: TRAIN_STEP_MS });
    }
    const k = Math.min(1, g / r.gens);
    const w = FINAL_W.map((x) => Math.round(x * k));
    const live = status === "running" && g < r.gens;
    const frac = live ? ((Date.now() - r.created) % TRAIN_STEP_MS) / TRAIN_STEP_MS : 0;
    const finished = status === "done" || status === "failed" || status === "cancelled";
    const cfg = { name: r.name, population: r.pop, elites: 6, generations: r.gens, hands_per_candidate: r.hpc, unit_size: 5000,
      opponents: ["heuristic", "heuristic", "heuristic"], init: r.init, sigma0: 20, smoothing: 0.7, min_sigma: 2,
      seed: r.seed, validation_hands: 200000, rules: { no_points_first_trick: true } };
    const endC = curveAt(r, r.gens - 1);
    const out = {
      id: r.id, name: r.name, status, config: cfg, seed: r.seed, generation: g, generations_total: r.gens,
      current: live ? { generation: g, candidates_total: r.pop, candidates_done: Math.floor(frac * r.pop),
        hands_done: Math.round(frac * r.pop * r.hpc), hands_total: r.pop * r.hpc } : null,
      champion: g >= 1 ? { strategy: "linear:" + w.join(","), fitness: +curveAt(r, g - 1).mean.toFixed(3),
        weights: FEATURES.map((f, i) => ({ feature: f, weight: w[i] })) } : null,
      validation: status === "done" ? { hands: 200000, champion_fitness: +(endC.mean - 0.01).toFixed(2),
        baseline_fitness: 6.49, champion_job_id: 90 + r.id * 2, baseline_job_id: 91 + r.id * 2 } : null,
      created_at: iso(r.created), finished_at: finished ? iso(r.finishedAt || r.created + 3600e3) : null, last_error: null,
    };
    if (withGens) out.generations = gens;
    return out;
  }

  const nodes = ["k8s-w1", "k8s-w2", "k8s-w3"];
  function workers() {
    const now = Date.now(), list = [];
    for (let i = 0; i < 7; i++) {
      const stale = i === 6, name = `hearts-worker-7d9f-${"abcdefg"[i]}x${i}k`;
      list.push({ worker_id: name, pod_name: name, node_name: nodes[i % 3], threads: i % 3 === 0 ? 4 : 2,
        first_seen: iso(now - 3600e3), last_seen: iso(stale ? now - 95e3 : now - (i * 1300 + 400)), active: !stale,
        leased_units: stale ? 0 : (jobs[0].status === "running" ? 1 : 0) });
    }
    return list;
  }

  // Deterministic pseudo-score per card so the same hand gives stable bars.
  function advise(b) {
    const hand = b.hand || [];
    if (!hand.length) throw new Error("hand must not be empty");
    const samples = b.samples || 1000, partial = samples > 20000;
    const achieved = partial ? Math.round(samples * 0.6) : samples;
    const moves = hand.map((c) => {
      let h = 0; for (const ch of c) h = (h * 31 + ch.charCodeAt(0)) % 97;
      const base = c === "QS" ? 9.8 : c[1] === "H" ? 3 + h / 40 : 0.5 + h / 30;
      return { card: c, avg_points: +base.toFixed(2), samples: achieved };
    }).sort((a, c) => a.avg_points - c.avg_points);
    return { best: moves[0].card, moves, samples_requested: samples, samples_achieved: achieved, partial,
      shards: 3, shards_failed: partial ? 1 : 0, latency_ms: 80 + Math.round(samples / 100) };
  }

  // `champion:N` -> that run's current champion (the real API does this server-side).
  function resolveChampion(s) {
    const m = /^champion:(\d+)$/.exec(s);
    if (!m) return s;
    const r = runs.find((x) => x.id === +m[1]);
    if (!r) throw new Error(`training run ${m[1]} not found`);
    const c = trainView(r, false).champion;
    if (!c) throw new Error(`training run ${m[1]} has no champion yet`);
    return c.strategy;
  }

  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  async function handle(method, path, body) {
    await wait(60);
    const p = new URL(path, "http://x").pathname;
    let m;
    if (method === "GET" && p === "/api/workers") return workers();
    if (method === "GET" && p === "/api/jobs") return jobs.slice().reverse().map(view);
    if (method === "GET" && p === "/api/training") return runs.slice().reverse().map((r) => trainView(r, false));
    if (method === "POST" && p === "/api/training") {
      const b = body || {};
      const r = { id: runs.length + 1, name: b.name || `run ${runs.length + 1}`, gens: b.generations || 30, pop: b.population || 24,
        hpc: b.hands_per_candidate || 40000, init: b.init || "heuristic", seed: b.seed ?? 1234, created: Date.now(), done: false };
      runs.push(r);
      return trainView(r, true);
    }
    if ((m = p.match(/^\/api\/training\/(\d+)(\/cancel)?$/))) {
      const r = runs.find((x) => x.id === +m[1]);
      if (!r) throw new Error("training run not found");
      if (m[2] && !r.done && !r.status) { r.frozen = trainGen(r); r.status = "cancelled"; r.finishedAt = Date.now(); }
      return trainView(r, true);
    }
    if (method === "POST" && p === "/api/jobs") {
      body = { ...body, strategies: body.strategies.map(resolveChampion) };
      const j = { id: jobs.length + 1, name: body.name || `job ${jobs.length + 1}`, strategies: body.strategies,
        num_hands: body.num_hands, unit_size: body.unit_size || 2000, seed: body.seed ?? 1234,
        status: "running", created: Date.now(), started: Date.now(), finished: null, fixed: null };
      jobs.push(j);
      return view(j);
    }
    if ((m = p.match(/^\/api\/jobs\/(\d+)(\/cancel)?$/))) {
      const j = jobs.find((x) => x.id === +m[1]);
      if (!j) throw new Error("job not found");
      if (m[2] && j.status === "running") { j.fixed = frac(j); j.status = "cancelled"; j.finished = Date.now(); }
      return view(j);
    }
    if (method === "POST" && p === "/api/advise") return advise(body);
    throw new Error(`mock: no route for ${method} ${p}`);
  }

  // Fake SSE: progress every second, then one "done" when the job is terminal. Returns a close function.
  function sse(path, onProgress, onDone) {
    const m = path.match(/^\/api\/jobs\/(\d+)\/events$/);
    const j = jobs.find((x) => x.id === +(m && m[1]));
    let timer = null;
    const tick = () => {
      if (!j) return;
      const v = view(j);
      if (v.status === "running" || v.status === "pending") onProgress(v);
      else { clearInterval(timer); onDone(v); }
    };
    timer = setInterval(tick, 1000);
    setTimeout(tick, 50);
    return () => clearInterval(timer);
  }
  window.HEARTS_MOCK = { handle, sse };
})();
