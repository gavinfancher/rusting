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
  const AVG = { random: 7.4, heuristic: 5.6, pimc: 4.9 };

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

  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  async function handle(method, path, body) {
    await wait(60);
    const p = new URL(path, "http://x").pathname;
    let m;
    if (method === "GET" && p === "/api/workers") return workers();
    if (method === "GET" && p === "/api/jobs") return jobs.slice().reverse().map(view);
    if (method === "POST" && p === "/api/jobs") {
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
