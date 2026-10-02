#!/usr/bin/env python3
"""Percentiles and tables from the raw logs in out/local/ (bench/local.sh) -- prints Markdown. Usage: bench/stats.py [dir]"""
import glob, json, os, re, sys

D = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "..", "out", "local")
COMP = ["hello_p3", "hello_p2", "hello_js"]
CPUS = ["1.0", "0.29", "0.07"]
LAMBDA_MB = {"1.0": 1769, "0.29": 512, "0.07": 128}


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, max(0, -(-len(xs) * q // 100) - 1))] if xs else float("nan")


def lines(path):
    out = []
    for l in open(path, errors="replace"):
        l = l.strip()
        if l.startswith("{"):
            try:
                out.append(json.loads(l))
            except ValueError:
                pass
    return out


def ev(path, name):
    return next((e for e in lines(path) if e.get("event") == name), None)


def ms(us, d=1):
    return "-" if us is None else f"{us / 1000:.{d}f}"


def table(head, rows):
    print("| " + " | ".join(head) + " |")
    print("|" + "|".join("---" for _ in head) + "|")
    for r in rows:
        print("| " + " | ".join(str(c) for c in r) + " |")
    print()


def cold():
    print("#### Compile (Cranelift) at a CPU share, from the wasm blob\n")
    rows = []
    for c in COMP:
        for cpus in CPUS:
            p = f"{D}/cold-{c}-{cpus}-default-compile.log"
            if not os.path.exists(p):
                continue
            ld, st = ev(p, "load"), ev(p, "status")
            ok = ld is not None and st["http"] == "200"
            rows.append([c, f"{cpus} (~{LAMBDA_MB[cpus]} MB)", f"{ld['wasm_bytes'] // 1000}" if ld else "-",
                         f"{ld['cwasm_bytes'] // 1000}" if ld else "-", ms(ld["compile_us"]) if ld else "-",
                         f"{ld['rss_kb'] // 1024}" if ld else "-", "ok" if ok else f"FAILED (http {st['http']}, oom-killed {st['oom']})"])
    table(["component", "Docker --cpus (approx. Lambda memory)", "wasm KB", "cwasm KB", "compile ms", "RSS MB after", "result"], rows)

    print("#### Deserialize + first request from a cached .cwasm (local path spec, default allocator)\n")
    rows = []
    for c in COMP:
        for cpus in CPUS:
            p = f"{D}/cold-{c}-{cpus}-default-deserialize.log"
            if not os.path.exists(p):
                continue
            ld, rq, it = ev(p, "load"), next((e for e in lines(p) if e.get("kind") == "guest"), None), ev(p, "init")
            if not ld or not rq:
                rows.append([c, f"{cpus} (~{LAMBDA_MB[cpus]} MB)", "-", "-", "-", "-", "-", "-", "not measurable: no cached .cwasm (compile failed above)"]); continue
            pre = ld["instantiate_pre_us"] + ld["proxy_pre_us"]
            rows.append([c, f"{cpus} (~{LAMBDA_MB[cpus]} MB)", ms(it["init_total_us"]) if it else "-", ms(ld["deserialize_us"]), ms(pre), ms(rq["instantiate_us"]),
                         ms(rq["handle_us"]), ms(ld["load_total_us"]), f"{ld['rss_kb'] // 1024}"])
    table(["component", "Docker --cpus", "host init ms (process start to listening)", "deserialize ms", "instantiate_pre + ProxyPre ms", "1st instantiate ms",
           "1st handle ms", "load total ms", "RSS MB"], rows)

    print("#### Allocator: first request after deserialize at --cpus 1.0\n")
    rows = []
    for c in COMP:
        for a in ["default", "pooling"]:
            p = f"{D}/cold-{c}-1.0-{a}-deserialize.log"
            if not os.path.exists(p):
                continue
            ld, rq = ev(p, "load"), next((e for e in lines(p) if e.get("kind") == "guest"), None)
            if ld and rq:
                rows.append([c, a, ms(ld["deserialize_us"]), ms(ld["instantiate_pre_us"] + ld["proxy_pre_us"]), ms(rq["instantiate_us"], 2), ms(rq["handle_us"], 2)])
    table(["component", "allocator", "deserialize ms", "instantiate_pre ms", "1st instantiate ms", "1st handle ms"], rows)


def bucket():
    print("#### Cold path from the bucket (MinIO on localhost: fetch times are optimistic, not S3 times)\n")
    rows = []
    for c in COMP:
        for cpus in CPUS:
            for kind, label in [("blob", "blob + compile"), ("cwasm", "precompiled artifact"), ("zstd", "precompiled artifact, zstd copy")]:
                p = f"{D}/bucket-{c}-{cpus}-{kind}.log"
                if not os.path.exists(p):
                    continue
                ld, st, rq = ev(p, "load"), ev(p, "status"), next((e for e in lines(p) if e.get("kind") == "guest"), None)
                if not ld:
                    rows.append([c, f"{cpus} (~{LAMBDA_MB[cpus]} MB)", label, "-", "-", "-", "-", "-", "-", "-", f"FAILED http {st and st['http']}, oom-killed {st and st['oom']}"]); continue
                fetch = ld.get("fetch_cwasm_us", 0) + ld.get("fetch_zst_us", 0) if kind != "blob" else ld.get("fetch_us", 0)
                rows.append([c, f"{cpus} (~{LAMBDA_MB[cpus]} MB)", label, ms(fetch), (ld["zst_bytes"] if kind == "zstd" else ld["wasm_bytes"] if kind == "blob" else ld.get("cwasm_bytes", 0)) // 1000,
                             ms(ld.get("decompress_us")), ms(ld.get("compile_us", ld.get("deserialize_us"))), ms(ld.get("write_cache_us")),
                             ms(ld["load_total_us"]), ms(rq["total_us"]) if rq else "-", ld["route"]])
    table(["component", "Docker --cpus", "route", "fetch ms", "fetched KB", "zstd decompress ms", "compile or deserialize ms", "write /cache ms", "load total ms",
           "whole first request ms", "route taken"], rows)


def winch():
    print("#### Winch against Cranelift: the blob compiled in the host at a CPU share (empty cache, default allocator)\n")
    rows = []
    for c in COMP:
        for cpus in CPUS:
            for k in ["cranelift", "winch"]:
                p = f"{D}/compile-{k}-{c}-{cpus}.log"
                if not os.path.exists(p):
                    continue
                ld, st, rq = ev(p, "load"), ev(p, "status"), next((e for e in lines(p) if e.get("kind") == "guest"), None)
                if not ld or st["http"] != "200":
                    rows.append([c, f"{cpus} (~{LAMBDA_MB[cpus]} MB)", k, "-", "-", "-", "-", "-", "-", f"FAILED (http {st['http']}, oom-killed {st['oom']})"]); continue
                keep = ld["serialize_us"] + ld["write_cache_us"]
                rows.append([c, f"{cpus} (~{LAMBDA_MB[cpus]} MB)", k, ms(ld["compile_us"]), ms(keep), ld["cwasm_bytes"] // 1000, ld["rss_kb"] // 1024,
                             ms(ld["load_total_us"]), ms(ld["load_total_us"] - keep), ms(rq["total_us"]) if rq else "-"])
    table(["component", "Docker --cpus", "compiler", "compile ms", "serialize + cache write ms", "cwasm KB", "RSS MB after", "load total ms",
           "load total without the cache write ms", "whole first request ms"], rows)

    print("#### Winch against Cranelift: warm requests through the guest at --cpus 1.0 (c=1, default allocator, epoch interruption on in both)\n")
    rows = []
    for c in COMP:
        for k in ["cranelift", "winch"]:
            p = f"{D}/warm-{k}-{c}-1.0-default-c1"
            if not os.path.exists(p + "-oha.json"):
                continue
            o = oha(p + "-oha.json")
            reqs = [e for e in lines(p + ".log") if e.get("kind") == "guest"][30:]
            han, ins = [e["handle_us"] for e in reqs], [e["instantiate_us"] for e in reqs]
            rows.append([c, k, f"{o['p50']:.2f}", f"{o['p99']:.2f}", f"{o['rps']:.0f}", f"{pct(han, 50):.0f} / {pct(han, 99):.0f}", f"{pct(ins, 50):.0f} / {pct(ins, 99):.0f}"])
    table(["component", "compiler", "client p50 ms", "client p99 ms", "req/s", "guest handle p50/p99 µs", "instantiate p50/p99 µs"], rows)


def mac():
    print("#### MAC over a precompiled artifact: keyed BLAKE3 against HMAC-SHA256 (ms, p50 of 9 runs, with min to max; mac-soft-*.log is the same binary built without the sha2 asm feature)\n")
    rows = []
    for pre, sha in (("mac", "SHA2 instructions"), ("mac-soft", "software")):
        for cpus in ["1.0", "0.29"]:
            p = f"{D}/{pre}-{cpus}.log"
            r = ev(p, "mac-bench") if os.path.exists(p) else None
            for x in (r or {"results": []})["results"]:
                b, h = x["blake3_keyed_us"], x["hmac_sha256_us"]
                rows.append([f"{x['bytes'] / 1e6:.2f}", cpus, sha, ms(b[1], 2), f"{ms(b[0], 2)} to {ms(b[2], 2)}", ms(h[1], 2), f"{ms(h[0], 2)} to {ms(h[2], 2)}", f"{h[1] / b[1]:.1f}x"])
    table(["buffer MB", "--cpus", "SHA-256 backend", "BLAKE3 keyed", "min to max", "HMAC-SHA256", "min to max", "HMAC / BLAKE3"], rows)


def oha(path):
    j = json.load(open(path))
    lp = j["latencyPercentiles"]
    return {"p50": lp["p50"] * 1000, "p99": lp["p99"] * 1000, "p999": lp["p99.9"] * 1000, "rps": j["summary"]["requestsPerSec"], "ok": j["summary"]["successRate"]}


def warm():
    print("#### Warm requests through the guest (oha from another container; host columns are the host's own per-request log, µs)\n")
    rows = []
    for c in COMP:
        for cpus in CPUS:
            for a, k, q in [("default", 1, ""), ("default", 1, "q50"), ("default", 16, ""), ("pooling", 1, "")]:
                p = f"{D}/warm-{c}-{cpus}-{a}-c{k}{q}"
                if not os.path.exists(p + "-oha.json"):
                    continue
                o = oha(p + "-oha.json")
                reqs = [e for e in lines(p + ".log") if e.get("kind") == "guest"][30:]  # the first 30 are warm-up
                tot, han, ins = [e["total_us"] for e in reqs], [e["handle_us"] for e in reqs], [e["instantiate_us"] for e in reqs]
                rows.append([c, cpus, a, f"{k} @50/s" if q else k, f"{o['p50']:.2f}", f"{o['p99']:.2f}", f"{o['p999']:.2f}", f"{o['rps']:.0f}",
                             f"{pct(tot, 50):.0f} / {pct(tot, 99):.0f}", f"{pct(han, 50):.0f} / {pct(han, 99):.0f}", f"{pct(ins, 50):.0f} / {pct(ins, 99):.0f}",
                             f"{pct([t - h for t, h in zip(tot, han)], 50):.0f}"])
    table(["component", "--cpus", "allocator", "conc", "client p50 ms", "client p99 ms", "client p99.9 ms", "req/s", "host total p50/p99 µs",
           "guest handle p50/p99 µs", "instantiate p50/p99 µs", "host overhead p50 µs"], rows)
    p = f"{D}/warm-ready-oha.json"
    if os.path.exists(p):
        o = oha(p)
        print(f"Host-only route (`/__ready`, no guest) from the same client: p50 {o['p50']:.2f} ms, p99 {o['p99']:.2f} ms. "
              f"Everything above this floor in the client columns is the guest plus the host's own work.\n")


def lwa():
    print("#### Lambda Web Adapter overhead (RIE: invocation event in, HTTP to the host, response out)\n")
    rows = []
    for c in ["hello_p3", "hello_p2"]:
        p = f"{D}/lwa-{c}.log"
        if not os.path.exists(p):
            continue
        text = open(p, errors="replace").read()
        dur = [float(x) for x in re.findall(r"REPORT RequestId:.*?\tDuration: ([\d.]+) ms", text)]
        reqs = [e for e in lines(p) if e.get("kind") == "guest"]
        init = [f"{float(x):.1f}" for x in re.findall(r"INIT REPORT\(durationMs: ([\d.]+)\)", text)]
        hinit = ev(p, "init")
        n = min(len(dur) - 1, len(reqs) - 1)
        # the first invocation is the cold one; drop it and the 30 warm-up calls
        d, r = dur[-n:], reqs[-n:]
        over = [dd * 1000 - e["total_us"] for dd, e in zip(d[-3000:], r[-3000:])]
        rows.append([c, len(over), f"{pct([x * 1000 for x in d], 50):.0f} / {pct([x * 1000 for x in d], 99):.0f}",
                     f"{pct([e['total_us'] for e in r], 50):.0f} / {pct([e['total_us'] for e in r], 99):.0f}",
                     f"{pct(over, 50):.0f} / {pct(over, 99):.0f}", init[0] if init else "-", ms(hinit["init_total_us"]) if hinit else "-", f"{dur[0]:.1f}" if dur else "-"])
    table(["component", "warm invocations", "REPORT Duration p50/p99 µs", "host total p50/p99 µs", "adapter + RIE overhead p50/p99 µs",
           "INIT REPORT ms (cold: extension + host init + readiness)", "host init ms", "1st invocation Duration ms (includes Cranelift: no cache in this image)"], rows)


if __name__ == "__main__":
    for f in (cold, bucket, winch, mac, warm, lwa):
        try:
            f()
        except Exception as e:  # keep going: partial runs are normal
            print(f"({f.__name__}: {e!r})\n")
