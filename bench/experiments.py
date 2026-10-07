"""Run the same mistakes and situations through Django 6.1.2 and Rangoli and record what happens.

    python experiments.py      # writes results/experiments.json
"""
import json
import os
import re
import shutil
import subprocess
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

import run  # server start helpers from the benchmark

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
PY = str(HERE / ".venv/bin/python")
BIN = str(ROOT / "target/release/blog")
DJ = HERE / "django_blog"
out = {}


def sh(cmd, cwd=None, env=None):
    p = subprocess.run(cmd, cwd=cwd, env={**os.environ, **(env or {})}, capture_output=True, text=True)
    return p.returncode, (p.stdout + p.stderr).strip()


# 1. A typo in a query --------------------------------------------------------------
code, check = sh([PY, "manage.py", "check"], cwd=DJ, env={"DJANGO_DB": str(HERE / "results/django.sqlite3")})
code, err = sh([PY, "manage.py", "shell", "-c", "from blog.models import Post; Post.objects.filter(titel='Hello').count()"],
               cwd=DJ, env={"DJANGO_DB": str(HERE / "results/django.sqlite3")})
django_error = [l for l in err.splitlines() if "FieldError" in l][-1]
code_r, rust = sh(["cargo", "check", "--quiet"], cwd=HERE / "typo_rangoli", env={"CARGO_TARGET_DIR": str(ROOT / "target")})
rust_lines = [l for l in rust.splitlines() if l.strip()]
start = next(i for i, l in enumerate(rust_lines) if l.startswith("error"))
out["typo"] = {
    "django_check": check.splitlines()[-1],
    "django_runtime_error": django_error,
    "rangoli_compiles": code_r == 0,
    "rangoli_compiler_error": "\n".join(rust_lines[start:start + 8]),
}

# 2. Two branches each add a migration ------------------------------------------------
with tempfile.TemporaryDirectory() as tmp:
    proj = Path(tmp) / "django_blog"
    shutil.copytree(DJ, proj, ignore=shutil.ignore_patterns("__pycache__"))
    mig = proj / "blog/migrations"
    (mig / "0002_author_bio.py").write_text(
        "from django.db import migrations, models\n\nclass Migration(migrations.Migration):\n"
        "    dependencies = [('blog', '0001_initial')]\n"
        "    operations = [migrations.AddField('author', 'bio', models.TextField(default=''))]\n")
    (mig / "0002_tag_color.py").write_text(
        "from django.db import migrations, models\n\nclass Migration(migrations.Migration):\n"
        "    dependencies = [('blog', '0001_initial')]\n"
        "    operations = [migrations.AddField('tag', 'color', models.CharField(max_length=7, default='#000000'))]\n")
    code, text = sh([PY, "manage.py", "migrate"], cwd=proj, env={"DJANGO_DB": str(Path(tmp) / "dj.sqlite3")})
    django_branch = {"exit_code": code, "output": "\n".join(l for l in text.splitlines() if l.strip())[-400:]}

with tempfile.TemporaryDirectory() as tmp:
    migdir = Path(tmp) / "migrations"
    shutil.copytree(ROOT / "examples/blog/migrations", migdir)
    add = lambda table, col, ty: json.dumps({"operations": [{"op": "add_column", "table": table, "column": {"name": col, "type": ty}, "default": ""}]})
    (migdir / "20261007120000_add_blog_author_bio.json").write_text(add("blog_author", "bio", "text"))
    (migdir / "20261007120500_add_blog_tag_color.json").write_text(add("blog_tag", "color", {"varchar": 7}))
    env = {"RANGOLI_DATABASE_URL": f"sqlite://{tmp}/r.sqlite3?mode=rwc", "RANGOLI_MIGRATIONS": str(migdir)}
    code, text = sh([BIN, "migrate"], cwd=ROOT / "examples/blog", env=env)
    rangoli_branch = {"exit_code": code, "output": text}
    # A real conflict: both branches add the same column.
    (migdir / "20261007121000_add_blog_author_bio.json").write_text(add("blog_author", "bio", "text"))
    code, text = sh([BIN, "migrate"], cwd=ROOT / "examples/blog", env=env)
    rangoli_conflict = {"exit_code": code, "output": text}
out["branch_migrations"] = {"django": django_branch, "rangoli": rangoli_branch, "rangoli_real_conflict": rangoli_conflict}

# 3. N+1 queries -------------------------------------------------------------------------
script = """
import json
from django.db import connection
from django.test.utils import CaptureQueriesContext
from blog.models import Post
base = lambda: Post.objects.filter(published=True).order_by('-created_at')[:50]
def cards(qs):
    return [(p.title, p.author.name, [t.name for t in p.tags.all()]) for p in qs]
with CaptureQueriesContext(connection) as naive:
    cards(base())
with CaptureQueriesContext(connection) as tuned:
    cards(base().select_related('author').prefetch_related('tags'))
print(json.dumps({'naive': len(naive.captured_queries), 'tuned': len(tuned.captured_queries)}))
"""
code, text = sh([PY, "manage.py", "shell", "-c", script], cwd=DJ, env={"DJANGO_DB": str(HERE / "results/django.sqlite3")})
django_n1 = json.loads(text.splitlines()[-1])
code, text = sh([str(ROOT / "target/release/examples/count_queries")], env={"RANGOLI_DATABASE_URL": f"sqlite://{HERE}/results/rangoli.sqlite3"})
out["n_plus_one"] = {"django": django_n1, "rangoli": json.loads(text)}

# 4. Security defaults ----------------------------------------------------------------------
defaults = (HERE / "results/django_default_settings.py").read_text()
out["new_project_defaults"] = {
    "django_debug": re.search(r"^DEBUG = (.+)$", defaults, re.M).group(1),
    "django_secret_key_in_source": "SECRET_KEY = 'django-insecure-" in defaults,
    "django_csp_middleware": "ContentSecurityPolicyMiddleware" in defaults,
}


def headers(url):
    with urllib.request.urlopen(url) as r:
        keep = ["content-security-policy", "x-frame-options", "x-content-type-options", "referrer-policy", "cross-origin-opener-policy"]
        return {k: r.headers.get(k) for k in keep}


def attempt(url, user, pw, rangoli):
    """One login attempt; True when it logged in."""
    jar = urllib.request.HTTPCookieProcessor()
    opener = urllib.request.build_opener(jar, urllib.request.HTTPRedirectHandler())
    fields = {"username": user, "password": pw}
    hdrs = {"Sec-Fetch-Site": "same-origin"}
    if not rangoli:
        page = opener.open(url).read().decode()
        fields["csrfmiddlewaretoken"] = re.search(r'name="csrfmiddlewaretoken" value="([^"]+)"', page).group(1)
        fields["next"] = "/admin/"
        hdrs["Referer"] = url
    try:
        with opener.open(urllib.request.Request(url, data=urllib.parse.urlencode(fields).encode(), headers=hdrs)) as r:
            return r.geturl().rstrip("/").endswith("/admin"), r.status
    except urllib.error.HTTPError as e:
        return False, e.code


def cross_site_post(url):
    req = urllib.request.Request(url, data=b"username=a&password=b", headers={"Sec-Fetch-Site": "cross-site", "Origin": "https://evil.example"})
    try:
        return urllib.request.urlopen(req).status
    except urllib.error.HTTPError as e:
        return e.code


sec = {}
for name, start, base, login in [("rangoli", run.start_rangoli, run.RANGOLI, "/admin/login"), ("django", run.start_django, run.DJANGO, "/admin/login/")]:
    proc, _ = start()
    try:
        wrong = [attempt(base + login, "admin", f"guess-{i}", name == "rangoli")[1] for i in range(8)]
        right, status = attempt(base + login, "admin", "bench-pass-123", name == "rangoli")
        sec[name] = {
            "headers": headers(base + "/"),
            "eight_wrong_passwords": wrong,
            "correct_password_after_guessing_logs_in": right,
            "cross_site_login_post": cross_site_post(base + login),
        }
    finally:
        run.stop(proc)
out["security"] = sec

# 5. Background tasks ------------------------------------------------------------------------
code, cmds = sh([PY, "manage.py", "help", "--commands"], cwd=DJ)
code, tasks = sh([PY, "manage.py", "shell", "-c", "from django.conf import settings; print(settings.TASKS)"], cwd=DJ)
code, rhelp = sh([BIN, "help"], cwd=ROOT / "examples/blog")
out["tasks"] = {
    "django_task_backend": tasks.splitlines()[-1],
    "django_worker_commands": [c for c in cmds.split() if "task" in c or "worker" in c],
    "rangoli_worker_command": [l.strip() for l in rhelp.splitlines() if "worker" in l],
}

# 6. Footprint and size --------------------------------------------------------------------
def kb(path):
    return int(subprocess.run(["du", "-sk", str(path)], capture_output=True, text=True).stdout.split()[0])


def lines(files):
    n = 0
    for f in files:
        for l in Path(f).read_text().splitlines():
            s = l.strip()
            if s and not s.startswith(("#", "//")):
                n += 1
    return n


site = next((HERE / ".venv/lib").glob("python*/site-packages"))
out["footprint"] = {
    "rangoli_binary_mb": round(os.path.getsize(BIN) / 1e6, 1),
    "django_site_packages_mb": round(kb(site) / 1024, 1),
    "django_python_packages": len(list(site.glob("*.dist-info"))),
    "rangoli_app_lines": lines([ROOT / "examples/blog/src/main.rs", ROOT / "examples/blog/src/models.rs"]),
    "django_app_lines": lines([DJ / "blog/models.py", DJ / "blog/admin.py", DJ / "blog/views.py", DJ / "site_/urls.py", DJ / "site_/settings.py"]),
}

(HERE / "results/experiments.json").write_text(json.dumps(out, indent=2))
print(json.dumps(out, indent=2))
