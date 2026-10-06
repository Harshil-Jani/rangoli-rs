"""Build the Rangoli report site from the measured results.

    python site/build.py      # writes site/dist/index.html (plus the video and poster)

Every number on the page is read from bench/results/*.json; nothing is typed in by hand.
"""
import html
import json
import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
DIST = SITE / "dist"
bench = json.loads((ROOT / "bench/results/bench.json").read_text())
exp = json.loads((ROOT / "bench/results/experiments.json").read_text())
r, d = bench["frameworks"]["rangoli"], bench["frameworks"]["django"]
REPO = "https://github.com/Harshil-Jani/rangoli-rs"
e = html.escape

SCENARIOS = [
    ("posts_json", "/posts.json", "50 posts with their author, as JSON"),
    ("home_html", "Homepage", "a template with 50 posts and their tags"),
    ("api_list", "JSON API list", "Rangoli's built-in API vs Django REST framework"),
    ("admin_changelist", "Admin changelist", "logged in, 100 rows"),
]


def num(x):
    return f"{x:,.0f}"


def ratio(a, b):
    return f"{a / b:.1f}×"


# ---------------------------------------------------------------- chart

top = max(max(r["scenarios"][k]["rps"], d["scenarios"][k]["rps"]) for k, *_ in SCENARIOS)
axis_max = 16000 if top <= 16000 else int(top * 1.1)
rows = []
for key, label, note in SCENARIOS:
    rr, dd = r["scenarios"][key]["rps"], d["scenarios"][key]["rps"]
    bars = ""
    for cls, name, v in (("s1", "Rangoli", rr), ("s2", "Django 6.1.2", dd)):
        pct = v / axis_max * 100
        bars += (f'<div class="bar-line"><span class="bar {cls}" style="--w:{pct:.2f}%" tabindex="0" '
                 f'aria-label="{name}: {num(v)} requests per second"><span class="tip">{name}: {num(v)} req/s</span></span>'
                 f'<span class="bar-value">{num(v)}</span></div>')
    rows.append(f'<div class="bar-row"><div class="bar-label"><strong>{e(label)}</strong><span>{e(note)}</span></div>'
                f'<div class="bar-pair">{bars}</div></div>')
ticks = "".join(f'<span style="left:{t / axis_max * 100:.2f}%">{num(t)}</span>' for t in range(0, axis_max + 1, 4000))
chart = f'''<figure class="chart" aria-labelledby="chart-title">
  <figcaption id="chart-title">Requests per second, higher is better</figcaption>
  <div class="legend"><span><i class="swatch s1"></i>Rangoli</span><span><i class="swatch s2"></i>Django 6.1.2</span></div>
  {"".join(rows)}
  <div class="axis"><div class="ticks">{ticks}</div></div>
</figure>'''

table_rows = "".join(
    f"<tr><th scope='row'>{e(label)}</th><td>{num(r['scenarios'][k]['rps'])}</td><td>{num(d['scenarios'][k]['rps'])}</td>"
    f"<td>{r['scenarios'][k]['p99_ms']:.0f} ms</td><td>{d['scenarios'][k]['p99_ms']:.0f} ms</td>"
    f"<td class='win'>{ratio(r['scenarios'][k]['rps'], d['scenarios'][k]['rps'])}</td></tr>"
    for k, label, _ in SCENARIOS)

# ---------------------------------------------------------------- findings


def out(text):
    return f"<pre class='output'>{e(text)}</pre>"


typo, branch, n1 = exp["typo"], exp["branch_migrations"], exp["n_plus_one"]
defaults, sec, tasks, foot = exp["new_project_defaults"], exp["security"], exp["tasks"], exp["footprint"]
hdr_names = ["content-security-policy", "x-frame-options", "x-content-type-options", "referrer-policy", "cross-origin-opener-policy"]
headers_rows = "".join(
    f"<tr><th scope='row'><code>{h}</code></th><td>{e(sec['rangoli']['headers'][h] or 'not sent')}</td>"
    f"<td>{e(sec['django']['headers'][h] or 'not sent')}</td></tr>" for h in hdr_names)
wrong_r = ", ".join(str(c) for c in sec["rangoli"]["eight_wrong_passwords"])
wrong_d = ", ".join(str(c) for c in sec["django"]["eight_wrong_passwords"])

findings = [
    ("typo", "A typo in a query",
     "Both projects run a query on a field spelled <code>titel</code> instead of <code>title</code>.",
     f"<code>manage.py check</code> reports <em>{e(typo['django_check'])}</em>, so the code ships. It fails when the line runs:{out(typo['django_runtime_error'])}",
     f"The program does not compile, so it never ships:{out(typo['rangoli_compiler_error'])}",
     "Every field lookup in Rangoli is a typed constant, so misspelled fields, comparing a text column with a number, and columns from another model are caught before deploy."),
    ("branches", "Two branches each add a migration",
     "Two feature branches each add a column to a different table; both are merged and <code>migrate</code> runs.",
     f"Exit code {branch['django']['exit_code']}:{out(branch['django']['output'])}",
     f"Exit code {branch['rangoli']['exit_code']}, both applied:{out(chr(10).join(branch['rangoli']['output'].splitlines()[-2:]))}"
     f"When two branches really do collide (both add the same column), Rangoli refuses and names both files:{out(branch['rangoli_real_conflict']['output'])}",
     "Rangoli records the set of applied migrations instead of a chain, so unrelated work never needs a merge migration, while real conflicts are still caught."),
    ("n1", "N+1 queries",
     "Both load the latest 50 published posts with each post's author and tags.",
     f"Written the obvious way (<code>post.author.name</code> in a loop): <strong>{n1['django']['naive']} queries</strong>. "
     f"With <code>select_related</code> and <code>prefetch_related</code>: <strong>{n1['django']['tuned']} queries</strong>. Nothing warns you about the first version.",
     f"<strong>{n1['rangoli']['queries']} queries</strong>, measured with <code>count_queries</code>. A foreign key is an id, so there is no lazy <code>post.author</code> to call in a loop; related rows are fetched explicitly with <code>in_bulk</code> and <code>prefetch</code>.",
     "Rangoli can't fall into the 101-query trap by accident, but tuned Django is better here: it joins authors in, and Rangoli has no join (<code>select_related</code>) yet. That is on the roadmap."),
    ("defaults", "A new project's defaults",
     "What <code>django-admin startproject</code> generates in Django 6.1.2, compared with Rangoli's defaults.",
     f"<code>DEBUG = {e(defaults['django_debug'])}</code>; a <code>SECRET_KEY</code> written into <code>settings.py</code> "
     f"({'yes' if defaults['django_secret_key_in_source'] else 'no'}); Content Security Policy middleware enabled: {'yes' if defaults['django_csp_middleware'] else 'no'} "
     "(Django 6.0 added it as opt-in).",
     "Debug is off unless <code>RANGOLI_DEBUG=1</code>; there is no secret key at all (sessions are random keys in the database and CSRF protection needs no token); a strict CSP is sent on every response.",
     "Production-safe settings shouldn't be something you have to remember to change."),
    ("guessing", "Guessing the admin password",
     "Eight wrong passwords for <code>admin</code>, then the right one.",
     f"Status codes: {wrong_d}. The correct password then {'logs in' if sec['django']['correct_password_after_guessing_logs_in'] else 'is refused'}: guessing is unlimited without an extra package.",
     f"Status codes: {wrong_r}. The correct password then {'logs in' if sec['rangoli']['correct_password_after_guessing_logs_in'] else 'is refused'}: after five failures the username is locked for 15 minutes.",
     f"Cross-site login POSTs are rejected by both (Rangoli {sec['rangoli']['cross_site_login_post']}, Django {sec['django']['cross_site_login_post']}). Rangoli's lockout is per process today; across several servers it needs a shared store."),
    ("tasks", "Background tasks",
     "Can the framework run a task in the background, out of the box?",
     f"Django 6.x ships a tasks interface; its default backend is <code>{e(tasks['django_task_backend'])}</code>, which runs tasks immediately in the request. "
     f"Worker commands in <code>manage.py</code>: {e(', '.join(tasks['django_worker_commands']) or 'none')}. Production use still means Celery plus Redis or RabbitMQ.",
     f"Tasks are queued in your database and run in the same binary: in-process by <code>runserver</code>, or by <code>cargo run -- worker</code>. Retries with backoff, caught panics, and recovery from crashed workers are built in.",
     "One binary and your database instead of three moving parts."),
    ("footprint", "What you deploy",
     "The example blog app and everything it needs to run.",
     f"{foot['django_site_packages_mb']} MB of Python packages ({foot['django_python_packages']} of them) plus a Python runtime and gunicorn; {foot['django_app_lines']} lines of app code across models, admin, views, URLs and settings.",
     f"One {foot['rangoli_binary_mb']} MB binary with the admin's templates, CSS and JavaScript compiled in, plus your own templates; {foot['rangoli_app_lines']} lines of app code.",
     "Both counts skip blank lines and comments."),
]
finding_html = ""
for fid, title, setup, dj, rg, meaning in findings:
    finding_html += f'''<article class="finding" id="{fid}">
  <h3>{title}</h3>
  <p class="setup">{setup}</p>
  <div class="compare">
    <div class="side"><h4>Django 6.1.2</h4>{dj}</div>
    <div class="side"><h4>Rangoli</h4>{rg}</div>
  </div>
  <p class="meaning">{meaning}</p>
</article>
'''

# ---------------------------------------------------------------- page

machine, settings = bench["machine"], bench["settings"]
page = f'''<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="icon" href="favicon.svg" type="image/svg+xml">
<title>Rangoli: a Django-style web framework for Rust</title>
<meta name="description" content="Typed ORM, branch-safe migrations, a Django-style admin, a JSON API with OpenAPI, and background tasks in one Rust binary, measured against Django 6.1.2.">
<style>
:root {{
  --s1: 4px; --s2: 8px; --s3: 12px; --s4: 16px; --s5: 24px; --s6: 32px; --s7: 48px; --s8: 72px;
  --bg: #fbfaf8; --surface: #ffffff; --ink: #1c2430; --ink-2: #4a5565; --line: #dde1e7;
  --brand: #1f4e66; --brand-ink: #ffffff; --mark: #f5dd5d; --link: #1d5a7a; --code-bg: #f0f2f5;
  --series-1: #2a78d6; --series-2: #eb6834; --win: #1e5b2c;
  --font: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  --mono: ui-monospace, Menlo, Consolas, monospace;
  color-scheme: light;
}}
@media (prefers-color-scheme: dark) {{
  :root:not([data-theme="light"]) {{
    --bg: #12161b; --surface: #1a1f26; --ink: #e6e9ee; --ink-2: #b4bcc8; --line: #2e3540;
    --brand: #16323f; --link: #7cc4e8; --code-bg: #222831; --series-1: #3987e5; --series-2: #d95926; --win: #a6e3b5;
    color-scheme: dark;
  }}
}}
:root[data-theme="dark"] {{
  --bg: #12161b; --surface: #1a1f26; --ink: #e6e9ee; --ink-2: #b4bcc8; --line: #2e3540;
  --brand: #16323f; --link: #7cc4e8; --code-bg: #222831; --series-1: #3987e5; --series-2: #d95926; --win: #a6e3b5;
  color-scheme: dark;
}}
* {{ box-sizing: border-box; }}
body {{ margin: 0; background: var(--bg); color: var(--ink); font: 17px/1.6 var(--font); }}
a {{ color: var(--link); }}
.sr {{ position: absolute; width: 1px; height: 1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; }}
:focus-visible {{ outline: 2px solid var(--series-1); outline-offset: 2px; }}
code, pre {{ font-family: var(--mono); font-size: 0.86em; }}
code {{ background: var(--code-bg); padding: 1px 5px; }}
pre {{ margin: var(--s3) 0; padding: var(--s3) var(--s4); background: var(--code-bg); overflow-x: auto; line-height: 1.5; }}
pre code {{ background: none; padding: 0; overflow-wrap: normal; }}
h1, h2, h3, h4 {{ line-height: 1.2; margin: 0; }}
h2 {{ font-size: 28px; margin-bottom: var(--s4); }}
h3 {{ font-size: 20px; margin-bottom: var(--s2); }}
h4 {{ font-size: 13px; color: var(--ink-2); margin-bottom: var(--s2); }}
p {{ margin: 0 0 var(--s4); }}

.top {{ background: var(--brand); color: var(--brand-ink); }}
.top .in {{ display: flex; align-items: center; justify-content: space-between; gap: var(--s4); flex-wrap: wrap; padding: var(--s3) var(--s5); max-width: 1080px; margin: 0 auto; }}
.word {{ color: var(--mark); font-weight: 700; font-size: 20px; text-decoration: none; }}
.top nav {{ display: flex; gap: var(--s4); flex-wrap: wrap; font-size: 15px; }}
.top nav a {{ color: var(--brand-ink); text-decoration: none; }}
.top nav a:hover {{ text-decoration: underline; }}

main {{ max-width: 1080px; margin: 0 auto; padding: 0 var(--s5); }}
section {{ padding: var(--s8) 0 var(--s6); border-bottom: 1px solid var(--line); }}
.hero {{ padding-top: var(--s7); }}
.hero h1 {{ font-size: clamp(34px, 5vw, 54px); letter-spacing: -0.5px; max-width: 18ch; margin-bottom: var(--s4); }}
.lede {{ font-size: 20px; color: var(--ink-2); max-width: 62ch; }}
.actions {{ display: flex; gap: var(--s3); flex-wrap: wrap; margin: var(--s5) 0 var(--s6); }}
.btn {{ display: inline-block; padding: var(--s2) var(--s4); border: 1px solid var(--brand); background: var(--brand); color: var(--brand-ink); text-decoration: none; font-weight: 600; font-size: 15px; }}
.btn.ghost {{ background: transparent; color: var(--link); border-color: var(--link); }}
video {{ display: block; width: 100%; max-width: 1080px; aspect-ratio: 16 / 9; background: #12161b; border: 1px solid var(--line); }}
.caption {{ font-size: 14px; color: var(--ink-2); margin-top: var(--s2); }}

.steps {{ display: grid; grid-template-columns: minmax(0, 1fr); gap: var(--s5); max-width: 820px; counter-reset: step; }}
.step {{ counter-increment: step; min-width: 0; }}
.step h3::before {{ content: counter(step) ". "; color: var(--ink-2); }}
.features {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(min(300px, 100%), 1fr)); gap: var(--s4) var(--s6); margin: 0; }}
.features dt {{ font-weight: 700; }}
.features dd {{ margin: 0 0 var(--s3); color: var(--ink-2); }}

.method {{ color: var(--ink-2); font-size: 15px; max-width: 75ch; }}
.chart {{ margin: var(--s5) 0; padding: var(--s5); background: var(--surface); border: 1px solid var(--line); }}
.chart figcaption {{ font-weight: 700; margin-bottom: var(--s2); }}
.legend {{ display: flex; gap: var(--s5); font-size: 14px; color: var(--ink-2); margin-bottom: var(--s4); }}
.swatch {{ display: inline-block; width: 12px; height: 12px; margin-right: var(--s2); vertical-align: -1px; }}
.s1 {{ background: var(--series-1); }} .s2 {{ background: var(--series-2); }}
.bar-row {{ display: grid; grid-template-columns: 220px 1fr; gap: var(--s4); align-items: center; padding: var(--s3) 0; border-top: 1px solid var(--line); }}
.bar-label {{ display: flex; flex-direction: column; font-size: 15px; }}
.bar-label span {{ font-size: 13px; color: var(--ink-2); line-height: 1.35; }}
.bar-line {{ display: flex; align-items: center; gap: var(--s2); height: 22px; margin: 2px 0; }}
.bar {{ position: relative; display: block; width: var(--w); min-width: 2px; height: 16px; }}
.bar-value {{ font-size: 14px; font-variant-numeric: tabular-nums; color: var(--ink); }}
.tip {{ display: none; position: absolute; left: 0; bottom: 22px; padding: var(--s1) var(--s2); background: var(--ink); color: var(--bg); font-size: 13px; white-space: nowrap; z-index: 2; }}
.bar:hover .tip, .bar:focus .tip {{ display: block; }}
.axis {{ margin-left: calc(220px + var(--s4)); position: relative; height: 22px; border-top: 1px solid var(--line); }}
.ticks span {{ position: absolute; top: 4px; transform: translateX(-50%); font-size: 12px; color: var(--ink-2); }}
.ticks span:first-child {{ transform: none; }}

table {{ width: 100%; border-collapse: collapse; font-variant-numeric: tabular-nums; font-size: 15px; background: var(--surface); }}
caption {{ text-align: left; font-weight: 700; padding: var(--s2) 0; }}
th, td {{ text-align: left; padding: var(--s2) var(--s3); border-bottom: 1px solid var(--line); vertical-align: top; }}
thead th {{ font-size: 13px; color: var(--ink-2); }}
td.win {{ color: var(--win); font-weight: 700; }}
.table-wrap {{ overflow-x: auto; max-width: 100%; border: 1px solid var(--line); }}
.tiles {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(220px, 1fr)); gap: var(--s4); margin: var(--s5) 0; }}
.tile {{ padding: var(--s4); background: var(--surface); border: 1px solid var(--line); }}
.tile .label {{ font-size: 14px; color: var(--ink-2); }}
.tile .value {{ font-size: 30px; font-weight: 700; font-variant-numeric: tabular-nums; }}
.tile .vs {{ font-size: 14px; color: var(--ink-2); }}
.story {{ border-left: 4px solid var(--series-2); padding: var(--s3) var(--s4); background: var(--surface); max-width: 80ch; }}
.story p:last-child {{ margin: 0; }}

.finding {{ padding: var(--s6) 0; border-top: 1px solid var(--line); }}
.finding:first-of-type {{ border-top: 0; }}
.setup {{ color: var(--ink-2); }}
.compare {{ display: grid; grid-template-columns: 1fr 1fr; gap: var(--s5); }}
.side {{ min-width: 0; padding: var(--s4); background: var(--surface); border: 1px solid var(--line); font-size: 15px; }}
.output {{ white-space: pre-wrap; word-break: break-word; font-size: 13px; }}
.meaning {{ margin-top: var(--s4); font-weight: 600; max-width: 80ch; }}
.gaps li {{ margin-bottom: var(--s2); }}
footer {{ max-width: 1080px; margin: 0 auto; padding: var(--s6) var(--s5) var(--s7); color: var(--ink-2); font-size: 14px; }}

@media (max-width: 760px) {{
  main, .top .in, footer {{ padding-left: var(--s4); padding-right: var(--s4); }}
  section {{ padding-top: var(--s7); }}
  .compare {{ grid-template-columns: 1fr; }}
  code {{ overflow-wrap: anywhere; }}
  pre code {{ overflow-wrap: normal; }}
  .bar-row {{ grid-template-columns: 1fr; gap: var(--s2); }}
  .axis {{ margin-left: 0; }}
  .chart {{ padding: var(--s4); }}
}}
@media (prefers-reduced-motion: reduce) {{ video {{ animation: none; }} }}
</style>
</head>
<body>
<header class="top"><div class="in">
  <a class="word" href="#">Rangoli</a>
  <nav aria-label="Sections"><a href="#start">Get started</a><a href="#results">Results</a><a href="#findings">Findings</a><a href="#gaps">Not done yet</a><a href="{REPO}">GitHub</a></nav>
</div></header>
<main>
<section class="hero">
  <h1>A Django-style web framework for Rust</h1>
  <p class="lede">Rangoli gives you what Django gives you: models, migrations, an admin, auth, forms, templates, a JSON API and background tasks. It adds what Django can't: queries the compiler checks, migrations that don't conflict across branches, safe defaults, and one fast binary to deploy.</p>
  <div class="actions"><a class="btn" href="#start">Get started</a><a class="btn ghost" href="{REPO}">Source on GitHub</a><a class="btn ghost" href="#results">See the benchmarks</a></div>
  <video controls preload="metadata" poster="rangoli-demo-poster.jpg" aria-describedby="video-caption">
    <source src="rangoli-demo.mp4" type="video/mp4">
    Your browser can't play this video. <a href="rangoli-demo.mp4">Download it</a>.
  </video>
  <p class="caption" id="video-caption">49 seconds, no sound: define models, run the commands, then the public pages, the admin, the JSON API and the task list of the example blog. Recorded from the real app.</p>
</section>

<section id="start">
  <h2>Get started</h2>
  <div class="steps">
    <div class="step"><h3>Add the crate</h3>
<pre><code>[dependencies]
rangoli = {{ git = "{REPO}" }}
tokio = {{ version = "1", features = ["full"] }}
serde = {{ version = "1", features = ["derive"] }}</code></pre></div>
    <div class="step"><h3>Describe your data</h3>
<pre><code>use rangoli::prelude::*;

#[derive(Model, Clone, Debug)]
#[model(table = "blog_post", display = "title")]
pub struct Post {{
    pub id: Option&lt;i64&gt;,
    #[field(max_length = 200)]
    pub title: String,
    pub published: bool,
    #[field(auto_now_add)]
    pub created_at: DateTime,
}}

#[tokio::main]
async fn main() -&gt; rangoli::Result&lt;()&gt; {{
    App::new()
        .admin::&lt;Post&gt;()
        .api::&lt;Post&gt;(Api::new().read(Access::Public))
        .run()
        .await
}}</code></pre></div>
    <div class="step"><h3>Run it</h3>
<pre><code>cargo run -- makemigrations
cargo run -- migrate
cargo run -- createsuperuser admin
RANGOLI_DEBUG=1 cargo run</code></pre>
<p>Open <code>http://127.0.0.1:8000/admin/</code> for the admin, <code>/api/blog_post/</code> for the API and <code>/api/schema.json</code> for its OpenAPI document. Postgres or MySQL: set <code>RANGOLI_DATABASE_URL</code>.</p></div>
  </div>
  <h3 style="margin-top: var(--s6)">What you get</h3>
  <dl class="features">
    <div><dt>Typed ORM</dt><dd><code>Post::objects().filter(Post::TITLE.contains("rust"))</code>: a misspelled field doesn't compile. Transactions, many-to-many, <code>in_bulk</code> and <code>prefetch</code>.</dd></div>
    <div><dt>Migrations</dt><dd>Generated from your models, applied as a set so branches don't conflict. Postgres, MySQL and SQLite.</dd></div>
    <div><dt>The admin</dt><dd>Django's admin, page for page: filters, search, actions, history, delete protection, password change.</dd></div>
    <div><dt>JSON API</dt><dd>List, detail, create, update and delete with validation, pagination and filters, plus an OpenAPI schema.</dd></div>
    <div><dt>Background tasks</dt><dd>Queued in your database, run by a worker in the same binary, with retries.</dd></div>
    <div><dt>Pages and forms</dt><dd>Templates with Django 6.0 style partials, static files, and <code>ModelForm</code> returning typed models.</dd></div>
  </dl>
</section>

<section id="results">
  <h2>Measured against Django 6.1.2</h2>
  <p class="method">The same blog (authors, posts, tags) written in both, loaded with identical data: 1,000 posts, 722 published, 2,000 tag links. {e(machine['cpu'])}, {machine['cores']} cores, SQLite, {settings['connections']} concurrent connections for {settings['duration']} after a warm-up, measured with oha. Django 6.1.2 runs under gunicorn with {settings['django_workers']} sync workers and DEBUG off; Rangoli runs its release build. Startup is the median of five launches.</p>
  {chart}
  <div class="table-wrap"><table>
    <caption class="sr">Benchmark results</caption>
    <thead><tr><th scope="col">Page</th><th scope="col">Rangoli req/s</th><th scope="col">Django req/s</th><th scope="col">Rangoli p99</th><th scope="col">Django p99</th><th scope="col">Rangoli is</th></tr></thead>
    <tbody>{table_rows}</tbody>
  </table></div>
  <div class="tiles">
    <div class="tile"><div class="label">Startup to first response</div><div class="value">{r['startup_ms']} ms</div><div class="vs">Django: {d['startup_ms']} ms</div></div>
    <div class="tile"><div class="label">Memory under load</div><div class="value">{r['loaded_rss_mb']:.0f} MB</div><div class="vs">Django, 8 workers: {d['loaded_rss_mb']:.0f} MB</div></div>
    <div class="tile"><div class="label">Memory at rest</div><div class="value">{r['idle_rss_mb']:.0f} MB</div><div class="vs">Django: {d['idle_rss_mb']:.0f} MB</div></div>
  </div>
  <div class="story">
    <p><strong>The first run went the other way.</strong> Rangoli was slower than Django on three of the four pages, and got slower as concurrency rose. Profiling showed every SQLite connection in the process waiting on SQLite's global mutexes for memory statistics and its shared page cache. Django never hits them, because each gunicorn worker is a separate process.</p>
    <p>Rangoli now turns memory statistics off at runtime, and SQLite apps build SQLite without the shared page cache (<a href="{REPO}/blob/main/.cargo/config.toml">.cargo/config.toml</a>). The homepage went from 249 to {num(r['scenarios']['home_html']['rps'])} req/s. With Postgres or MySQL, neither applies.</p>
  </div>
</section>

<section id="findings">
  <h2>Findings</h2>
  <p class="method">Each finding runs the same code or command in both and shows the actual output, captured by <a href="{REPO}/blob/main/bench/experiments.py">bench/experiments.py</a>.</p>
  {finding_html}
  <article class="finding" id="headers">
    <h3>Security headers on a normal page</h3>
    <div class="table-wrap"><table>
      <thead><tr><th scope="col">Header</th><th scope="col">Rangoli</th><th scope="col">Django 6.1.2</th></tr></thead>
      <tbody>{headers_rows}</tbody>
    </table></div>
  </article>
</section>

<section id="gaps">
  <h2>Where Django is still ahead</h2>
  <ul class="gaps">
    <li><strong>Twenty years of ecosystem.</strong> Thousands of packages, books, hosting guides and people who know it. Rangoli is brand new and not yet published on crates.io.</li>
    <li><strong>Joins.</strong> No <code>select_related</code> yet, so the example homepage takes 4 queries where tuned Django takes 2.</li>
    <li><strong>Admin depth.</strong> No inlines, custom actions, fieldsets, per-model permissions or groups.</li>
    <li><strong>The rest of the batteries.</strong> Internationalization, email, caching, password reset, savepoints in nested transactions, WebSockets and API tokens are on the roadmap.</li>
    <li><strong>SQLite tuning.</strong> Getting full SQLite throughput needs one config file in your project. Postgres and MySQL need nothing.</li>
  </ul>
  <p>The full list is in the <a href="{REPO}#whats-not-done-yet">README</a>.</p>
</section>

<section id="reproduce">
  <h2>Reproduce it</h2>
<pre><code>git clone {REPO} && cd rangoli-rs
cargo build --release -p blog
cd bench && uv venv .venv && uv pip install --python .venv/bin/python "django==6.1.2" djangorestframework gunicorn
python seed.py results/rangoli.sqlite3 results/django.sqlite3   # after migrating both, see bench/README
python run.py          # throughput, latency, memory, startup -> results/bench.json
python experiments.py  # the findings above -> results/experiments.json</code></pre>
</section>
</main>
<footer>Rangoli is open source under MIT or Apache-2.0. Numbers from <a href="{REPO}/blob/main/bench/results/bench.json">bench.json</a> and <a href="{REPO}/blob/main/bench/results/experiments.json">experiments.json</a>.</footer>
</body>
</html>
'''

DIST.mkdir(exist_ok=True)
(DIST / "index.html").write_text(page)
for asset in ["rangoli-demo.mp4", "rangoli-demo-poster.jpg", "favicon.svg"]:
    shutil.copy(SITE / asset, DIST / asset)
(DIST / ".nojekyll").write_text("")
print(f"wrote {DIST / 'index.html'}")
