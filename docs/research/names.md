# Naming candidates for "spinit" (fact sheet)

> A research sub-agent gathered this on 2026-10-01. Registry status comes from live API calls (crates.io, npm registry and its downloads API, formulae.brew.sh, GitHub search and users API) plus web searches for products and companies. Domains were skipped on purpose.
> General-knowledge claims that were not re-verified are marked UNVERIFIED. No trademark search was done.

**Bottom line:** `torpor` is the best fit by meaning and is clean in the Wasm, cloud and serverless space. `lull` and `drowse` are the best short alternatives. Most short English words are already squatted on crates.io, so "free on every registry" is rare; the practical test is whether a notable, active project owns the name.

## What the name has to carry

A tiny single Rust binary that runs WASI HTTP components on Lambda, later Azure Functions and Cloud Run, with all state in an object-storage bucket and about $0.05/month idle. The names that tested best lean on dormancy (idle costs nothing, wake on request) or on the bucket as a small physical container.

## Ranked shortlist (12)

Registry status checked 2026-10-01. "Free" means the API returned 404. Fallback crate names `torpor-cli`, `lull-cli`, `drowse-cli` and `tardi-cli` were all free.

| # | Name | Letters | crates.io | Notable collisions | CLI example |
|---|---|---|---|---|---|
| 1 | **torpor** | 6 | Taken by a fresh, unrelated crate: 0.1.0, 23 downloads, created 2026-06-20 (PoW time-lock puzzle) | None notable. An academic paper, "Torpor: GPU-Enabled Serverless Computing" (arXiv 2306.03622), but no OSS product | `torpor deploy ./hello.wasm` |
| 2 | **lull** | 4 | Taken by an abandoned sound player: 1.0.2, 3.4k downloads, last release 2020-10-27 | No devtool. Many consumer apps and a mattress company called Lull (SEO and trademark crowding); near-homophone of Dutch slang (UNVERIFIED) | `lull deploy ./hello.wasm` |
| 3 | **drowse** | 6 | Free | `figsoda/drowse`, a Nix dynamic-derivations tool (107 stars, active); otherwise none | `drowse deploy ./hello.wasm` |
| 4 | **larder** | 6 | Free | Larder.io, a "bookmarking for developers" SaaS with API and CLI | `larder deploy ./hello.wasm` |
| 5 | **tardi** | 5 | Free | None for `tardi`. "Tardigrade" is crowded (Storj's S3-compatible storage, a Wasm workflow engine, a TS agent framework); `tardi` can read as "tardy" | `tardi deploy ./hello.wasm` |
| 6 | **creel** | 5 | Free | None notable (tiny repos only, including a browser-Wasm agent tool) | `creel deploy ./hello.wasm` |
| 7 | **dormant** | 7 | Free | None notable (tiny repos only) | `dormant deploy ./hello.wasm` |
| 8 | **emmer** | 5 | Free | `TimoKats/emmer`, a tiny (20 stars) JSON-over-storage API; otherwise none | `emmer deploy ./hello.wasm` |
| 9 | **pipkin** | 6 | Free | `aivarannamaa/pipkin` (21 stars, a pip-like tool for MicroPython) | `pipkin deploy ./hello.wasm` |
| 10 | **dormouse** | 8 | Free | DiffPlug's Dormouse terminal app: active, hosted relay at relay.dormouse.sh, `dor` CLI | `dormouse deploy ./hello.wasm` |
| 11 | **lambkin** | 7 | Free | PyPI `lambkin` and `ninaspitfire/lambkin`, a deprecated AWS Lambda CLI (same space, dead) | `lambkin deploy ./hello.wasm` |
| 12 | **pittance** | 8 | Free | None found | `pittance deploy ./hello.wasm` |

## Detail per shortlisted name

### 1. torpor
- **Fit:** the state animals enter to save energy while alive and ready to wake. That is the scale-to-zero cost story in one word.
- **crates.io:** `torpor` is taken, but by a brand-new crate (`41Baloo/torpor`, 2 stars) that is unrelated. The binary can still be `torpor` with the crate published as `torpor-cli`.
- **npm:** taken, abandoned ("lazy and monadic file systems", 1 download/week, last modified 2022). **Homebrew:** formula and cask free. **PyPI:** free.
- **GitHub:** biggest exact-name repo is `s4-lab-cuhksz/torpor` (22 stars, Python, probably the paper's code). No `torpor` hits for Wasm, serverless or Lambda queries.

### 2. lull
- **Fit:** a quiet gap between bursts of activity, which is what the platform is between requests. Four letters is the best CLI ergonomics on the list.
- **crates.io:** taken by an old looping sound player (3,376 downloads, repo on tinybird.dev, dormant since 2020). `lull-cli` is free.
- **npm:** taken, abandoned ("Simple RESTful Web Service", 2 downloads/week, 2022). **Homebrew:** formula and cask free.
- **GitHub:** only `octalmage/lull` (13 stars, Hulu ad muter) and `BlakeWilliams/lull` (5 stars).
- **Caveats:** web search shows several consumer products named Lull (sleep sounds, baby sleep tracker, a mattress company founded 2015), so search results will be noisy. It is one letter from Dutch slang "lul" (UNVERIFIED), which may raise a smile.

### 3. drowse
- **Fit:** half asleep, wakes when poked. Slightly less precise than torpor but friendly.
- **Registries:** crates.io, npm and Homebrew all free.
- **GitHub:** `figsoda/drowse` (107 stars, Nix, pushed 2026-09-29) is the only devtool; `a9lim/drowse` (9 stars) is an ML tool. Web search found no company or product.

### 4. larder
- **Fit:** a cool store where everything keeps; the bucket is the larder and the functions are the cooks. Strongest bucket metaphor of the free names.
- **crates.io:** free. **npm:** taken, abandoned (10 downloads/week, 2022). **Homebrew:** free.
- **Collision:** Larder.io, a developer bookmarking service with a public API, browser extensions and a third-party CLI (`theycallmemac/larder`, 20 stars). A GitHub org named `larder` ("Larder Project") also exists. Different job, same audience.

### 5. tardi
- **Fit:** short for tardigrade. A dehydrated tardigrade sits in a "tun" state (the same word as a large cask) and revives with water (UNVERIFIED, general knowledge). A mascot is easy.
- **Registries:** crates.io, npm, Homebrew and PyPI all free. GitHub has no repo named `tardi`.
- **Caveats:** needs explaining, and "tardi" reads like "tardy", which is the wrong connotation for latency. The longer `tardigrade` is crowded: Storj's S3-compatible storage product, `slowli/tardigrade` (a Wasm workflow engine, 16 stars) and `clavia-labs/tardigrade` (296 stars).

### 6. creel
- **Fit:** a fisher's wicker basket (a small container) and, in textile spinning, the frame that feeds bobbins, which nods to the "spin" heritage (UNVERIFIED, general knowledge).
- **Registries:** crates.io, npm and Homebrew free; PyPI taken.
- **GitHub:** `rsiota/creel` (6 stars, SQL TUI), `scbrown/creel` (browser agents with a Wasm sandbox, tiny), `Creel-ai/creel` (2 stars). Nothing notable.

### 7. dormant
- **Fit:** literal. Easy to explain, but generic and hard to search for.
- **crates.io:** free. **npm:** taken, abandoned (6 downloads/week). **Homebrew:** free. **PyPI:** free.
- **GitHub:** `legion-works/dormant` (2 stars, an OLED blanking daemon) and a few others; nothing notable.

### 8. emmer
- **Fit:** Dutch for "bucket", and an ancient wheat. A quiet bucket-in-another-language option.
- **Registries:** crates.io, npm and Homebrew free; PyPI taken.
- **GitHub:** `TimoKats/emmer` (20 stars, Go, a self-hosted JSON API over storage providers) is adjacent but tiny; `dropbox/emmer` (16 stars, archived 2014).
- **Caveat:** needs a one-line explanation, and it is a typo away from "ember".

### 9. pipkin
- **Fit:** a small earthenware pot, so a tiny container. No dormancy link.
- **Registries:** crates.io and npm free, Homebrew free.
- **GitHub:** `aivarannamaa/pipkin` (21 stars, installs packages for MicroPython) and `madeleineostoja/pipkin` (2 stars). The "pip-" prefix may suggest a Python tool.

### 10. dormouse
- **Fit:** the classic hibernator (and the sleepy guest at the Mad Hatter's tea party). Strong mascot, long for a CLI at 8 letters.
- **crates.io:** free. **npm:** taken, abandoned (19 downloads/week, 2022). **Homebrew:** free.
- **Collision:** `diffplug/dormouse`, a terminal app from DiffPlug with a `dor` CLI, many recent PRs and a hosted relay at relay.dormouse.sh. Only 5 stars so far, but it is an active devtool with the exact name. Also `dbostian/dormouse` (46 stars, C, 2022).

### 11. lambkin
- **Fit:** a "little lamb", a pun on Lambda. Cute, but it points at AWS only, and the roadmap includes Azure and GCP.
- **Registries:** crates.io, npm and Homebrew free. PyPI `lambkin` 0.3.5 is a CLI "for managing functions in AWS Lambda" (`ninaspitfire/lambkin`; web search says deprecated).
- **GitHub:** `Ekumen-OS/lambkin` (15 stars, Python). The PyPI and GitHub tool is in the same space, even if dead.

### 12. pittance
- **Fit:** a tiny amount of money, which is the pitch ($0.05/month idle). Self-aware humor.
- **Registries:** crates.io, npm and Homebrew free. GitHub has no repo with that name.
- **Caveats:** 8 letters, and "pittance" often means "inadequate pay", which may read as negative.

## Top 3

1. **torpor:** it names the exact mechanism (cheap dormancy that wakes on demand), is easy to type and pronounce, and has no notable project in Wasm, cloud or devtools; the only wrinkle is that the crate name is taken by a 23-download unrelated crate, so publish as `torpor-cli`.
2. **lull:** at four letters it is the best CLI command on the list and the word means exactly the idle gap between requests; the cost is crowded search results (consumer sleep apps, a mattress brand) and an abandoned `lull` crate.
3. **drowse:** it is the cleanest of the three on registries (crates.io, npm and Homebrew all free, no company), and "half asleep, wakes when poked" is a good brand voice; the only real collision is a 107-star Nix tool.

## Rejected

Reason is the strongest single collision or defect found.

**Collides with a notable project or product**
- **kiln:** `Kiln-AI/Kiln` (5.1k stars, AI dev platform); also a `kiln` GitHub org with 66 repos.
- **fallow:** `fallow-rs/fallow` (5.0k stars, Rust-based JS codebase tool), npm `fallow` at about 1.6M downloads/week, and a Homebrew formula.
- **marmot:** `maxpert/marmot` (2.8k stars, distributed SQLite) and `marmotdata/marmot` (618 stars); Homebrew formula exists.
- **hoard:** the Hoard memory allocator (1.3k stars), `Hyde46/hoard` Rust CLI (661 stars) and a crate with 10.6k downloads.
- **tarn:** npm `tarn` (about 9.1M downloads/week, a resource pool); crate `tarn` is an API testing CLI.
- **pail:** `laravel/pail` (926 stars) and `storacha/pail` (48 stars, a DAG key-value store); crate `pail` says "pail is now michi".
- **wisp:** `gleam-wisp/wisp` (1.5k stars, web framework) and `mbrock/wisp` (306 stars, Lisp in WebAssembly).
- **cairn:** `oritera/Cairn` (3.2k stars, AI search engine) and several other repos.
- **ember:** Ember.js (npm `ember`); the tiny crate is irrelevant next to that.
- **cinder:** OpenStack Cinder, block storage (UNVERIFIED, general knowledge); Homebrew cask `cinder` exists.
- **cask, keg, cellar:** Homebrew vocabulary; formula `cask` exists; crate `keg` is an active container tool (17.9k downloads, 2026-08); crate `cellar` has 10.7k downloads.
- **siesta:** Bryntum Siesta, a JS testing tool since 2009 (web search); crate `siesta` has 15.6k downloads.
- **slumber:** `slumber` is a terminal HTTP client crate (5.3.0, 67k downloads, active 2026-05) and has a Homebrew formula.
- **kip:** `elotl/kip` (233 stars, a Kubernetes virtual-kubelet provider), and only 3 letters.
- **hearth:** four devtool repos of 97 to 352 stars (a Rust server monitor, a threat-hunting repo, a Scala macro library, an Obsidian homepage).
- **catnap:** a crate named `catnap` is a "Visual CLI sleep" tool (same shape of binary), plus `iinsertNameHere/catnap` (295 stars, Nim).
- **spore:** crate squat ("Coming soon...", 2019), four unrelated GitHub repos of 52 to 86 stars, a game and a protocol; too overloaded.

**Collides in the Wasm, cloud or storage space (smaller)**
- **wadi:** crate `wadi` is "A device interface for wasi" (5.4k downloads, 2020).
- **wasmlet:** crate `wasmlet` is an embeddable WebAssembly engine (1.1k downloads, 2025).
- **cistern:** crate `cistern` is an async storage abstraction layer (2026-09) and `nbedos/cistern` is a CI TUI (174 stars).
- **firkin:** crate `firkin` is a containerization library (2026-05) and `jimsynz/firkin` is an Elixir S3 server; both are tiny but in this neighbourhood.
- **bellows:** crate is a durable task framework (2026-09).
- **stasis:** crate is a Wayland idle manager; npm `stasis` is a minimal Wasm runtime (tiny).
- **thunk:** crate for lazy evaluation, npm `thunk` at 3.8k/week, and a strong association with redux-thunk (UNVERIFIED).

**Generic, taken or crowded on registries**
- **sprig, skiff, mote, loom, sluice, atto, nadir, lethe, vesper, hush, thaw, doze, snooze, hod, den, tun, pilot, tinder, kindle:** crates and/or npm already used by active or high-download projects (`loom` 68M downloads, `sluice` 16.8M, `tun` 2.7M, `thaw` 143k with a Homebrew cask), or Homebrew formulas (`snooze`), or plain brands (Tinder, Kindle).
- **skein, spindle, treadle, bobbin:** textile names for the "spin" heritage, but `skein` (382k downloads) and `spindle` (1.1M downloads) are taken, `treadle` is a workflow engine, and `bobbin` is a "Reserved" squat with no dormancy or bucket link.
- **barn, burrow:** generic; `arokor/barn` has 401 stars and Homebrew has a `burrow` formula.
- **somnus:** crate squat ("Reserving the name, sorry..."), weak elsewhere.

**Free, but fail the pronounce, type or spell test**
- **estivate, hibernal, bivouac, situla, stoup, piggin, pannier:** all free on crates.io, but obscure, misspellable or awkward to type.
- **sopor:** free everywhere, but "sopor" means rubbish in Swedish (UNVERIFIED).
- **smoulder:** UK and US spelling split (smolder), and `smolder` is an SMB tool crate.
- **farthing:** crates.io free, but `farthing` is already used by several tiny Claude-Code and AI cost-meter repos plus an npm MCP server, so it is crowded with your likely audience.
- **latent, idler:** free on crates.io, but "latent" is ML noise and "idler" connotes loafing.

## Everything considered

torpor, lull, siesta, dormouse, marmot, estivate, drowse, doze, slumber, spore, pail, firkin, cask, tarn, larder, cellar, hod, keg, cistern, wadi, crock, creel, wisp, mote, sprig, skiff, coracle, kiln, ember, cinder, tinder, kindle, pilot, den, burrow, cairn, lichen, bivouac, bobbin, loom, nib, tun, pipkin, noggin, cubby, nook, stasis, quiesce, wick, kip, snooze, catnap, somnus, dormant, latent, hush, nadir, hearth, smelt, ingot, lambkin, scuttle, pannier, stoup, piggin, thaw, fallow, hoard, repose, hammock, barn, granary, pantry, midge, atto, washi, tardi, tardigrade, waterbear, dew, rill, emmer, situla, brume, sloth, cryo, yawn, winks, hibernal, sluice, idler, laze, loll, thunk, winkle, lethe, hypnos, vesper, sandman, sleeper, smoulder, bellows, amphora, pithos, cruse, growler, lullaby, tarry, otium, sopor, skein, spindle, treadle, farthing, mite, doit, groat, obol, pittance, smidge, crumb, smolder, wasmlet, frugal, thrifty, miser, torpid.

## Method and gaps

- crates.io: `GET /api/v1/crates/<name>` with a descriptive User-Agent, 1 request per second. 404 means free.
- npm: `registry.npmjs.org/<name>` plus the weekly-downloads API. Many npm packages show a mid-2022 modified date (likely a bulk registry touch, UNVERIFIED), so "abandoned" here means very low weekly downloads, not a confirmed dead repo.
- Homebrew: `formulae.brew.sh/api/formula/<name>.json` and `/cask/<name>.json`.
- PyPI: only existence was checked (the JSON had control characters, so details were not parsed except for `lambkin`).
- GitHub: top exact-name repos by stars plus user/org existence. Search for "<name> + wasm/serverless/lambda" returned no meaningful hits for torpor, lull, drowse or larder.
- Not done: trademark databases, non-English slang beyond the two flagged items, Go modules, Docker Hub, apt and winget names.
