"""Load both servers with django-bolt's own benchmark settings and write RESULTS.md.

    python bench/vs_bolt/run.py [bolt_url] [rangoli_url]

bombardier -c 100 -d 10s -l, a 500-request warmup, each endpoint measured in
ROUNDS rounds with the two servers interleaved; the table shows the median round.
Both servers must already be running against the same seeded database (see README).
"""
import json, os, platform, statistics, subprocess, sys, urllib.request

BOLT, RANGOLI = (sys.argv[1:3] + ["http://127.0.0.1:9001", "http://127.0.0.1:9002"][len(sys.argv[1:3]):])
ROUNDS = int(os.environ.get("ROUNDS", "3"))
SECONDS = int(os.environ.get("SECONDS", "10"))
BOMBARDIER = os.environ.get("BOMBARDIER", os.path.expanduser("~/go/bin/bombardier"))
ENDPOINTS = ["/json-1k", "/json-10k", "/db", "/articles?page=1&page_size=20", "/articles/1", "/auth/me", "/auth/articles?page=1&page_size=20"]


def login(base):
    req = urllib.request.Request(base + "/auth/login", method="POST", headers={"Content-Type": "application/json"},
                                 data=json.dumps({"username": "benchuser", "password": "benchpass123"}).encode())
    cookie = [c for c in urllib.request.urlopen(req, timeout=60).headers.get_all("Set-Cookie") if c.startswith("access_token=")]
    return cookie[0].split(";")[0]


def bombard(url, cookie, args):
    cmd = [BOMBARDIER, *args, "--print", "r", "--format", "json", url] + (["-H", f"Cookie: {cookie}"] if cookie else [])
    return json.loads(subprocess.run(cmd, capture_output=True, text=True, check=True).stdout)["result"]


def measure(base, path, cookie):
    url = base + path
    bombard(url, cookie, ["-c", "10", "-n", "500"])
    r = bombard(url, cookie, ["-c", "100", "-d", f"{SECONDS}s", "-l"])
    ok = r["req2xx"]
    bad = sum(r[k] for k in ("req1xx", "req3xx", "req4xx", "req5xx", "others"))
    pct = r["latency"]["percentiles"]
    return {"rps": r["rps"]["mean"], "p50": pct["50"] / 1000, "p99": pct["99"] / 1000, "errors": bad, "ok": ok}


cookies = {BOLT: login(BOLT), RANGOLI: login(RANGOLI)}
results = {(b, p): [] for b in (BOLT, RANGOLI) for p in ENDPOINTS}
for rnd in range(ROUNDS):
    for path in ENDPOINTS:
        for base in ((BOLT, RANGOLI) if rnd % 2 == 0 else (RANGOLI, BOLT)):
            m = measure(base, path, cookies[base] if path.startswith("/auth") else None)
            results[(base, path)].append(m)
            print(f"round {rnd + 1} {base} {path}: {m['rps']:,.0f} req/s p99 {m['p99']:.1f} ms errors {m['errors']}", flush=True)


def median(base, path):
    runs = sorted(results[(base, path)], key=lambda m: m["rps"])
    return runs[len(runs) // 2], sum(m["errors"] for m in runs)


def cpu_name():
    try:
        return subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip()
    except FileNotFoundError:  # Linux
        names = [l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")]
        return names[0] if names else platform.processor()


cpu = cpu_name()
lines = [
    "# Rangoli vs django-bolt, on django-bolt's own benchmark",
    "",
    f"Machine: {cpu}, {os.cpu_count()} cores, {platform.system()}. PostgreSQL 17 in Docker, seeded by the harness's own `seed_db.py`.",
    f"Load: bombardier -c 100 -d {SECONDS}s, 500-request warmup, {ROUNDS} rounds with the servers interleaved; median round shown.",
    "Both servers: one process, 8 database connections, access logs off.",
    "",
    "| Endpoint | django-bolt req/s | Rangoli req/s | Rangoli ÷ bolt | bolt p99 | Rangoli p99 | non-2xx (bolt / Rangoli) |",
    "|---|---:|---:|---:|---:|---:|---:|",
]
for path in ENDPOINTS:
    (b, be), (r, re) = median(BOLT, path), median(RANGOLI, path)
    lines.append(f"| `{path}` | {b['rps']:,.0f} | {r['rps']:,.0f} | {r['rps'] / b['rps']:.1f}× | {b['p99']:.1f} ms | {r['p99']:.1f} ms | {be} / {re} |")
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "RESULTS.md")
open(out, "w").write("\n".join(lines) + "\n")
print("\n".join(lines))
