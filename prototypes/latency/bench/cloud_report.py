#!/usr/bin/env python3
"""Markdown tables from the files bench/cloud.sh collects in out/cloud/ (CloudWatch lines per run, curl timings, bench responses).
Usage: bench/cloud_report.py [dir]    (default out/cloud). Written for the cloud run; it only reads local files."""
import glob, json, os, re, sys

D = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "..", "out", "cloud")
REPORT = re.compile(r"REPORT RequestId:\s*(\S+)\s+Duration:\s*([\d.]+) ms\s+Billed Duration:\s*(\d+) ms\s+Memory Size:\s*(\d+) MB\s+"
                    r"Max Memory Used:\s*(\d+) MB(?:\s+Init Duration:\s*([\d.]+) ms)?")
TARGETS = {"cold p99 (init + first request)": 500.0, "warm read p50": 30.0, "acknowledged write p99": 200.0}


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, max(0, -(-len(xs) * q // 100) - 1))] if xs else float("nan")


def stat(xs, d=1):
    return "-" if not xs else f"{pct(xs, 50):.{d}f} / {pct(xs, 99):.{d}f}"


def parse(path):
    reports, events = [], []
    for line in open(path, errors="replace"):
        m = REPORT.search(line)
        if m:
            reports.append({"id": m[1], "duration": float(m[2]), "billed": int(m[3]), "mem": int(m[4]), "max_mem": int(m[5]),
                            "init": float(m[6]) if m[6] else None})
        elif line.lstrip().startswith("{"):
            try:
                events.append(json.loads(line))
            except ValueError:
                pass
    return reports, events


def csv_times(path):
    """Client-side seconds from curl (reference only): [(http code, ms)]."""
    out = []
    if os.path.exists(path):
        for l in open(path):
            p = l.split()
            if len(p) == 3:
                out.append((p[1], float(p[2]) * 1000))
    return out


def table(head, rows):
    if not rows:
        return
    print("| " + " | ".join(head) + " |")
    print("|" + "|".join("---" for _ in head) + "|")
    for r in rows:
        print("| " + " | ".join(str(c) for c in r) + " |")
    print()


def label_parts(label):  # cold-hello_p3-precompiled-eager-512 -> (component, mode, memory)
    p = label.split("-")
    return p[1], "-".join(p[2:-1]), p[-1]


def cold():
    rows, verdict = [], []
    for path in sorted(glob.glob(f"{D}/cold-*.log")):
        label = os.path.basename(path)[:-4]
        comp, mode, mem = label_parts(label)
        reports, events = parse(path)
        colds = [r for r in reports if r["init"] is not None]
        if not colds:
            continue
        total = [r["init"] + r["duration"] for r in colds]
        ev = lambda name, key: [e[key] for e in events if e.get("event") == name and key in e]
        ms = lambda us: [x / 1000 for x in us]
        http = csv_times(f"{D}/{label}.csv")
        bad = sum(1 for c, _ in http if c != "200")
        rows.append([comp, mode, mem, len(colds), stat([r["init"] for r in colds]), stat([r["duration"] for r in colds]), stat(total),
                     f"{max(total):.1f}", stat(ms(ev("load", "load_total_us"))), stat(ms(ev("load", "deserialize_us") or ev("load", "compile_us")), 1),
                     stat(ms(ev("load", "fetch_cwasm_us") or ev("load", "fetch_zst_us") or ev("load", "fetch_us")), 1),
                     stat(ms(ev("load", "decompress_us")), 1), stat(ms(ev("init", "init_total_us")), 1),
                     stat([r["max_mem"] for r in colds], 0), stat([t for _, t in http]), bad or ""])
        if mode in ("precompiled", "precompiled-zstd") and comp in ("hello_p3", "hello_js"):
            verdict.append((comp, mode, mem, pct(total, 99)))
    print("#### Cold starts (REPORT: Init Duration and Duration of the first request, ms; p50 / p99)\n")
    table(["component", "mode", "MB", "n", "Init", "Duration (1st request)", "Init + Duration", "max", "host load total", "deserialize or compile",
           "fetch from bucket", "zstd decompress", "host init", "max memory MB", "client curl (reference)", "non-200"], rows)
    for comp, mode, mem, p99 in verdict:
        print(f"- {comp} {mode}, {mem} MB: p99 of Init + Duration {p99:.0f} ms, "
              f"{'meets' if p99 <= TARGETS['cold p99 (init + first request)'] else 'misses'} the 500 ms target (indicative when n < 100).")
    print()


def warm():
    rows = []
    for path in sorted(glob.glob(f"{D}/warm-*.log")):
        label = os.path.basename(path)[:-4]
        comp, mem = label.split("-")[1], label.split("-")[-1]
        reports, events = parse(path)
        reports = [r for r in reports if r["init"] is None]
        reqs = [e for e in events if e.get("event") == "req" and e.get("kind") == "guest"]
        n = min(len(reports), len(reqs))
        over = [r["duration"] * 1000 - e["total_us"] for r, e in zip(reports[:n], reqs[:n])]  # adapter, runtime API, and the rest of the platform
        http = csv_times(f"{D}/{label}.csv")
        rows.append([comp, mem, len(reports), len(reqs), stat([r["duration"] for r in reports], 2), stat([e["total_us"] / 1000 for e in reqs], 2),
                     stat([e["handle_us"] / 1000 for e in reqs], 2), stat([e["instantiate_us"] / 1000 for e in reqs], 2),
                     stat([o / 1000 for o in over], 2), stat([t for _, t in http])])
    print("#### Warm requests through the guest (ms; p50 / p99)\n")
    table(["component", "MB", "REPORT lines", "host lines", "REPORT Duration", "host total", "guest handle", "instantiate",
           "Duration minus host total (adapter overhead)", "client curl (reference)"], rows)
    print("The host and REPORT lines are paired by order; if the two counts differ the overhead column is unreliable. "
          "Adapter overhead above ~5 ms warm p50 (or ~50 ms cold) is the trigger for switching to lambda_http.\n")


def bench():
    rows, per = [], {}
    for path in sorted(glob.glob(f"{D}/bench-*.json")):
        _, mem, op = os.path.basename(path)[:-5].split("-", 2)
        try:
            j = json.load(open(path))
        except ValueError:
            continue
        s = [x / 1000 for x in j["samples_us"]]
        per[(int(mem), op)] = s
        rows.append([op, mem, len(s), f"{s[0]:.1f}", f"{pct(s, 50):.1f}", f"{pct(s, 99):.1f}", f"{max(s):.1f}"])
    rows.sort(key=lambda r: (r[0], int(r[1])))
    print("#### Storage operations from inside the function (ms; 1 KB object unless the name says 80kb or 800kb; sequential; the function was warmed with 5 calls first)\n")
    table(["operation", "MB", "n", "first", "p50", "p99", "max"], rows)
    for mem in sorted({m for m, _ in per}):
        g = lambda op, q: pct(per[(mem, op)], q) if (mem, op) in per else None
        lines = [("bucket KV read, revalidation (conditional GET, 304)", g("s3-get-304", 50), TARGETS["warm read p50"], "p50"),
                 ("bucket KV read, plain GET", g("s3-get", 50), TARGETS["warm read p50"], "p50"),
                 ("bucket KV write, create-if-absent", g("s3-put-create", 99), TARGETS["acknowledged write p99"], "p99"),
                 ("bucket KV write, If-Match update", g("s3-put-update", 99), TARGETS["acknowledged write p99"], "p99"),
                 ("DynamoDB read, strongly consistent", g("ddb-get-strong", 50), TARGETS["warm read p50"], "p50"),
                 ("DynamoDB write, conditional", g("ddb-put-cond", 99), TARGETS["acknowledged write p99"], "p99")]
        lines += [(f"bucket state object, {kb} KB, {what}", g(f"{op}-{kb}kb", 50), TARGETS["warm read p50"], "p50")
                  for kb in (80, 800) for op, what in (("s3-get", "plain GET"), ("s3-get-304", "revalidation (304)"))]
        print(f"At {mem} MB:")
        for name, v, target, q in lines:
            if v is not None:
                print(f"- {name}: {q} {v:.1f} ms vs {target:.0f} ms, {'meets' if v <= target else 'misses'}")
        print()


if __name__ == "__main__":
    for f in (cold, warm, bench):
        f()
