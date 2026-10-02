#!/usr/bin/env python3
"""Runaway-guest checks: does the host stop a guest that computes forever (10 s epoch deadline) or grows past the 256 MiB cap?

Usage: bench/runaway.py [--cpus 1.0] [--mem 1769m] [--workers N] [--spins 1,4] [compiler:component ...]   (default: the four combinations below)
  e.g. bench/runaway.py --workers 2 winch:hello_loop cranelift:hello_loop_p2
--workers sets TOKIO_WORKER_THREADS (bench/runaway.compose.yaml); without it the host uses one worker per CPU it is allowed (1 at --cpus 1.0).
--spins lists how many concurrent `/spin` requests to send in separate scenarios.

Every scenario recreates the host container, sends the runaway request(s) and probes `/` on the same host while they run.
One JSON object per scenario is printed and appended to out/runaway/<cpus>-w<workers>.jsonl.
Needs out/spinit-host and out/hello_loop*.wasm (docker/build-host.sh, components/build.sh). Docker only, no cloud.
"""
import http.client, json, os, re, subprocess, sys, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PORT = int(os.environ.get("SPINIT_HOST_PORT", "28080"))
ENV = {**os.environ, "COMPOSE_PROJECT_NAME": "spinit-spike-winch", "SPINIT_HOST_PORT": str(PORT),
       "MINIO_HOST_PORT": "29000", "DDB_HOST_PORT": "28000", "LWA_HOST_PORT": "29001"}
LIMIT = 60  # seconds to wait for a runaway request (the deadline is 10 s); a guest that is never stopped shows as "timeout"


def run(*cmd, env=ENV):
    return subprocess.run(cmd, cwd=ROOT, env=env, capture_output=True, text=True)


def get(path, timeout=LIMIT):
    """One request on a new connection: (status | "timeout" | error name, seconds, start of the body)."""
    t = time.time()
    try:
        c = http.client.HTTPConnection("localhost", PORT, timeout=timeout)
        c.request("GET", path)
        r = c.getresponse()
        return r.status, round(time.time() - t, 3), r.read(80).decode(errors="replace")
    except TimeoutError:
        return "timeout", round(time.time() - t, 3), ""
    except Exception as e:
        return type(e).__name__, round(time.time() - t, 3), str(e)[:80]


def start(compiler, component, cpus, mem, workers):
    env = {**ENV, "HOST_CPUS": cpus, "HOST_MEM": mem, "SPINIT_COMPONENT": f"/out/{component}.wasm", "SPINIT_COMPILER": compiler}
    files = ["-f", "compose.yaml"] + (["-f", "bench/runaway.compose.yaml"] if workers else [])
    if workers:
        env["TOKIO_WORKER_THREADS"] = workers
    run("docker", "compose", *files, "up", "-d", "--force-recreate", "--no-deps", "host", env=env)
    while get("/__ready", 1)[0] != 200:
        time.sleep(0.2)
    get("/", 60)  # the first request loads the component; every request after this is warm
    return run("docker", "compose", "ps", "-q", "host").stdout.strip()


def stats(cid):
    return run("docker", "stats", "--no-stream", "--format", "{{.CPUPerc}} {{.MemUsage}}", cid).stdout.strip()


def proc(cid):
    """Worker threads, thread count and VmRSS / VmHWM (MiB) of the host process (PID 1 in its container); None for a container that died."""
    comm = run("docker", "exec", cid, "sh", "-c", "cat /proc/1/task/*/comm | sort | uniq -c").stdout.split()
    names = dict(zip(comm[1::2], map(int, comm[0::2])))
    status = run("docker", "exec", cid, "cat", "/proc/1/status").stdout.splitlines()
    mib = lambda k: next((int(l.split()[1]) // 1024 for l in status if l.startswith(k + ":")), None)  # None: the container is gone
    return {"workers": names.get("tokio-rt-worker"), "threads": sum(names.values()), "rss_mib": mib("VmRSS"), "hwm_mib": mib("VmHWM")}


def state(cid):
    return run("docker", "inspect", "-f", "{{.State.Running}} oom={{.State.OOMKilled}} restarts={{.RestartCount}}", cid).stdout.strip()


def log(cid):
    """What the host wrote after startup besides `/` requests: stderr without the backtrace frames (trap messages), and the other request lines."""
    out = run("docker", "logs", cid)
    return [l for l in out.stderr.splitlines() if l.strip() and not re.match(r"\s+\d+:", l)][:6] + [
        l for l in out.stdout.splitlines() if '"event":"req"' in l and '"path":"/"' not in l and "__ready" not in l]


def scenario(compiler, component, cpus, mem, workers, path, n=1):
    """n concurrent requests to a runaway path, while `/` is probed on the same host every 0.5 s (3 s timeout)."""
    cid = start(compiler, component, cpus, mem, workers)
    out = {"compiler": compiler, "component": component, "cpus": cpus, "workers_set": workers, "path": path, "n": n, "proc_before": proc(cid)}
    t0, res, probes = time.time(), [None] * n, []

    def hit(i):
        r = get(path)
        res[i] = (r[0], round(time.time() - t0, 2), r[2][:60])

    def sample():
        time.sleep(5)
        out["stats_at_5s"] = stats(cid)

    ths = [threading.Thread(target=hit, args=(i,)) for i in range(n)]
    for t in ths + [threading.Thread(target=sample)]:
        t.start()
    time.sleep(0.5)
    while any(t.is_alive() for t in ths):
        s = round(time.time() - t0, 1)
        r = get("/", 3)
        probes.append((s, round(r[1] * 1000), r[0]))
        time.sleep(0.5)
    time.sleep(3)
    out["responses"] = res
    out["probes"] = probes  # (seconds since start, ms, status)
    out["stats_3s_after"] = stats(cid)
    after = get("/", 3)
    out["after"] = (after[0], round(after[1] * 1000, 1))
    out["state"], out["proc_after"], out["log"] = state(cid), proc(cid), log(cid)
    return out


def grow(compiler, component, cpus, mem, workers):
    """/grow twice (does the grown memory persist in a reused instance?), then /grow-abort (the allocation failure is a trap)."""
    cid = start(compiler, component, cpus, mem, workers)
    out = {"compiler": compiler, "component": component, "cpus": cpus, "workers_set": workers, "path": "/grow", "proc_before": proc(cid)}
    out["first"] = get("/grow")
    out["proc_after"] = proc(cid)
    out["second"] = get("/grow")
    out["after_abort"] = get("/grow-abort")
    out["proc_after_abort"] = proc(cid)
    out["after"] = get("/", 3)[:2]
    out["state"], out["log"] = state(cid), log(cid)
    return out


def scenarios(args, spins):
    for n in spins:
        yield scenario(*args, "/spin", n)
    yield scenario(*args, "/spin-calls")
    yield grow(*args)


def main():
    a, opt = sys.argv[1:], {"--cpus": "1.0", "--mem": "1769m", "--workers": None, "--spins": "1,4"}
    while a and a[0] in opt:
        opt[a[0]], a = a[1], a[2:]
    cpus, mem, workers = opt["--cpus"], opt["--mem"], opt["--workers"]
    spins = [int(n) for n in opt["--spins"].split(",")]
    combos = [c.split(":") for c in a] or [[c, p] for p in ("hello_loop", "hello_loop_p2") for c in ("cranelift", "winch")]
    os.makedirs(f"{ROOT}/out/runaway", exist_ok=True)
    with open(f"{ROOT}/out/runaway/{cpus}-w{workers or 'cpu'}.jsonl", "a") as f:
        for compiler, component in combos:
            for r in scenarios((compiler, component, cpus, mem, workers), spins):
                f.write(json.dumps(r) + "\n"), f.flush()
                print(json.dumps(r), flush=True)
    run("docker", "compose", "stop", "host")


main()
