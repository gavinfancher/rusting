# Agent D: Web UI

**Owns:** `web/` (everything in it).

## Goal
A simple, clean, fast operator UI served by the coordinator (embedded files, **no build step, no framework, no CDN**), so it works offline on a tailnet. It should make the distributed system *visible*: what each worker is doing, throughput, retries and reclaims, and partial advisor answers during chaos. Must work on a phone (16px side gutters, no horizontal scroll) and in light and dark mode (`prefers-color-scheme`).

## Files
`index.html`, `tournament.html`, `advisor.html`, `app.js` (shared helpers: fetch wrapper with error toasts, formatters, SSE helper, nav), `style.css` (CSS variables for theming), and `mock.js` (fixtures).

## Pages
1. **Cluster (`index.html`):** a workers table from `GET /api/workers` (pod, node, threads, active dot, leased units, last seen "3s ago"), polled every 2s. Group or color by node, so the owner can see pods spread across `k8s-w1..w3`. A summary strip: active workers, total threads, current cluster hands/sec (from the running job, if any), and the running job with a link. Links to Grafana (`https://grafana.k8s.ad.gavinf.com`; make the base URL a constant at the top of `app.js`).
2. **Tournaments (`tournament.html`):**
   - Launch form: 4 strategy selects (random / heuristic / pimc with a samples input), hands, unit size, seed (optional), name.
   - Job list from `GET /api/jobs`, with status badges.
   - Job detail (`?id=N`): subscribe to `/api/jobs/:id/events` SSE. Show a progress bar, unit-state counts as a stacked bar (pending/leased/done/failed), **reclaims** highlighted (non-zero means a worker died or was slow), elapsed time, and a live hands/sec sparkline (inline SVG, last 60 points). Then the standings table (strategy, avg points/hand, win %, moons, Q♠ taken), sorted by avg points, with the best highlighted. A cancel button.
3. **Advisor (`advisor.html`):**
   - Card picker: a 4×13 grid of card buttons with suit symbols ♣♦♠♥ and red/black colors. Modes: "my hand" (up to 13), "current trick" (up to 3, in order), "already played". The leader seat select and hearts-broken toggle are filled in automatically where possible (the trick containing hearts, or hearts in played).
   - A samples slider (100–50k) and a deadline input.
   - Submit to `POST /api/advise`. Results: a horizontal bar chart (inline SVG or CSS bars) of expected points per legal card with the best highlighted, plus `samples_achieved / samples_requested`, shards / shards_failed, latency, and a clear **"partial answer"** warning when `partial`.
   - A "random deal" button that fills a legal random hand (13 random cards, empty trick, leading with 2♣ if held) for quick demos.

## Mock mode
`?mock=1` on any page makes `app.js` return fixtures from `mock.js` instead of calling the API (including a fake SSE that advances progress), so you can develop and screenshot without a backend. Follow CONTRACTS.md shapes exactly.

## Quality bar
Accessible (labels, button semantics, focus states, sufficient contrast), no console errors, everything escaped (use `textContent`, not `innerHTML`, for API data), and a small amount of code (aim for under 1000 lines of JS total). A comment at the top of each file says what it does.

## Acceptance
- `python3 -m http.server -d web 8000`, then open `http://localhost:8000/?mock=1`, `/tournament.html?mock=1&id=1`, `/advisor.html?mock=1`. Every page renders with fixtures and no console errors. If Playwright or a headless Chrome is available, take screenshots and check them; otherwise at least run `node --check web/*.js`.
- Your final report lists every API call each page makes.
