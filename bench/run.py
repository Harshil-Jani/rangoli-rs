"""Benchmark the Rangoli example blog against the same blog in Django 6.1.

Both serve identical SQLite data (see seed.py). Rangoli runs its release binary
(one process, all cores); Django runs under gunicorn with one sync worker per
core and DEBUG off. Each scenario gets a warm-up, then a timed run with oha.

    python run.py            # writes results/bench.json
"""
import json
import os
import re
import subprocess
import time
import urllib.parse
import urllib.request
from http.cookiejar import CookieJar
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
RESULTS = HERE / "results"
CORES = os.cpu_count()
DURATION, WARMUP, CONNECTIONS = "15s", "3s", 64
RANGOLI, DJANGO, BOLT = "http://127.0.0.1:8100", "http://127.0.0.1:8200", "http://127.0.0.1:8300"


def wait_until_up(url, started):
    while True:
        try:
            with urllib.request.urlopen(url, timeout=1) as r:
                if r.status == 200:
                    return time.perf_counter() - started
        except Exception:
            time.sleep(0.01)


def start_rangoli():
    env = {**os.environ, "RANGOLI_DATABASE_URL": f"sqlite://{RESULTS}/rangoli.sqlite3", "RANGOLI_BIND": "127.0.0.1:8100", "RANGOLI_WORKERS": "0"}
    t = time.perf_counter()
    proc = subprocess.Popen([str(ROOT / "target/release/blog"), "runserver"], cwd=ROOT / "examples/blog", env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    return proc, wait_until_up(RANGOLI + "/posts.json", t)


def start_django():
    env = {**os.environ, "DJANGO_DB": str(RESULTS / "django.sqlite3")}
    t = time.perf_counter()
    proc = subprocess.Popen([str(HERE / ".venv/bin/gunicorn"), "site_.wsgi", "-w", str(CORES), "-b", "127.0.0.1:8200", "--log-level", "error"],
                            cwd=HERE / "django_blog", env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    return proc, wait_until_up(DJANGO + "/posts.json", t)


def startup_median(start, runs=5):
    """Median launch-to-first-response time; the first launch of a fresh binary can be slow on macOS."""
    times = []
    for _ in range(runs):
        proc, t = start()
        stop(proc)
        times.append(t)
    return sorted(times)[len(times) // 2]


def start_bolt():
    """django-bolt: Actix (Rust) HTTP server, Python handlers and the Django ORM, one process per core."""
    env = {**os.environ, "DJANGO_DB": str(RESULTS / "django.sqlite3")}
    t = time.perf_counter()
    proc = subprocess.Popen([str(HERE / ".venv/bin/python"), "manage.py", "runbolt", "--host", "127.0.0.1", "--port", "8300",
                             "--processes", str(CORES), "--skip-checks"],
                            cwd=HERE / "django_blog", env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    return proc, wait_until_up(BOLT + "/posts.json", t)


def stop(proc):
    """Stop a server and every process it spawned."""
    try:
        os.killpg(proc.pid, 15)
    except (ProcessLookupError, PermissionError):
        proc.terminate()
    proc.wait()


def tree_pids(pid):
    """A process and all its descendants."""
    pids, todo = [], [str(pid)]
    while todo:
        p = todo.pop()
        pids.append(p)
        todo += subprocess.run(["pgrep", "-P", p], capture_output=True, text=True).stdout.split()
    return pids


def rss_mb(pid):
    pids = tree_pids(pid)
    out = subprocess.run(["ps", "-o", "rss=", "-p", ",".join(pids)], capture_output=True, text=True).stdout.split()
    return round(sum(int(x) for x in out) / 1024, 1)


def login_rangoli():
    jar = CookieJar()
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
    data = urllib.parse.urlencode({"username": "admin", "password": "bench-pass-123"}).encode()
    opener.open(urllib.request.Request(RANGOLI + "/admin/login", data=data, headers={"Sec-Fetch-Site": "same-origin"}))
    return "; ".join(f"{c.name}={c.value}" for c in jar)


def login_django():
    jar = CookieJar()
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
    page = opener.open(DJANGO + "/admin/login/").read().decode()
    token = re.search(r'name="csrfmiddlewaretoken" value="([^"]+)"', page).group(1)
    data = urllib.parse.urlencode({"username": "admin", "password": "bench-pass-123", "csrfmiddlewaretoken": token, "next": "/admin/"}).encode()
    opener.open(urllib.request.Request(DJANGO + "/admin/login/", data=data, headers={"Referer": DJANGO + "/admin/login/"}))
    return "; ".join(f"{c.name}={c.value}" for c in jar)


def oha(url, cookie=None, duration=DURATION):
    cmd = ["oha", "-z", duration, "-c", str(CONNECTIONS), "--no-tui", "--output-format", "json", url]
    if cookie:
        cmd[1:1] = ["-H", f"Cookie: {cookie}"]
    out = json.loads(subprocess.run(cmd, capture_output=True, text=True, check=True).stdout)
    codes = out.get("statusCodeDistribution", {})
    lat = out["latencyPercentiles"]
    return {
        "rps": round(out["summary"]["requestsPerSec"], 1),
        "p50_ms": round(lat["p50"] * 1000, 2),
        "p99_ms": round(lat["p99"] * 1000, 2),
        "success_rate": out["summary"]["successRate"],
        "status_codes": codes,
    }


def check(url, cookie=None, expect=b""):
    req = urllib.request.Request(url, headers={"Cookie": cookie} if cookie else {})
    with urllib.request.urlopen(req) as r:
        body = r.read()
        assert r.status == 200 and expect in body, (url, r.status, body[:200])
        return len(body)


def main():
    # (rangoli path, django path, django-bolt path or None, needs login, expected text)
    scenarios = {
        "hello": ("/hello", "/hello", "/hello", False, b"Hello"),
        "posts_json": ("/posts.json", "/posts.json", "/posts-async.json", False, b"title"),  # bolt: its faster async handler
        "home_html": ("/", "/", None, False, b"<article>"),
        "api_list": ("/api/blog_post/?limit=50", "/api/posts/?limit=50", None, False, b"results"),
        "admin_changelist": ("/admin/blog_post/", "/admin/blog/post/", None, True, b"result_list"),
    }
    results = {"machine": {"cpu": subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip(), "cores": CORES},
               "settings": {"duration": DURATION, "connections": CONNECTIONS, "django_workers": CORES, "bolt_processes": CORES, "startup": "median of 5 launches"}, "frameworks": {}}
    for name, start, base, idx in [("rangoli", start_rangoli, RANGOLI, 0), ("django", start_django, DJANGO, 1), ("django_bolt", start_bolt, BOLT, 2)]:
        startup = startup_median(start)
        proc, _ = start()
        try:
            cookie = {"rangoli": login_rangoli, "django": login_django}.get(name, lambda: None)()
            fw = {"startup_ms": round(startup * 1000), "idle_rss_mb": rss_mb(proc.pid), "scenarios": {}}
            for key, paths in scenarios.items():
                path, needs_login, expect = paths[idx], paths[3], paths[4]
                if path is None:
                    continue
                c = cookie if needs_login else None
                size = check(base + path, c, expect)
                oha(base + path, c, WARMUP)
                fw["scenarios"][key] = {"path": path, "bytes": size, **oha(base + path, c)}
                print(name, key, fw["scenarios"][key]["rps"], "req/s")
            fw["loaded_rss_mb"] = rss_mb(proc.pid)
            results["frameworks"][name] = fw
        finally:
            stop(proc)
    (RESULTS / "bench.json").write_text(json.dumps(results, indent=2))
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
