"""Write the same authors, tags and posts into the Rangoli and Django databases."""
import random
import sqlite3
import sys
from datetime import datetime, timezone

rangoli_db, django_db = sys.argv[1], sys.argv[2]
rng = random.Random(42)
words = "engine relay compiler moth punch card loom tape vacuum tube transistor kernel lambda heap stack cache bus".split()
authors = [(i, f"Author {i}", f"author{i}@example.com") for i in range(1, 21)]
tags = [(i, w) for i, w in enumerate(["history", "hardware", "languages", "theory", "people", "ai",
                                       "networks", "databases", "security", "design", "math", "systems"], 1)]
posts, links = [], []
for i in range(1, 1001):
    title = " ".join(rng.choice(words) for _ in range(4)).capitalize() + f" #{i}"
    body = " ".join(rng.choice(words) for _ in range(60))
    ts = 1_760_000_000 + i * 3600
    posts.append((i, title, body, 1 if rng.random() < 0.7 else 0, rng.randint(1, 20), round(rng.uniform(1, 5), 1), ts))
    for t in rng.sample(range(1, 13), 2):
        links.append((i, t))

r = sqlite3.connect(rangoli_db)
r.executemany("INSERT INTO blog_author (id, name, email) VALUES (?, ?, ?)", authors)
r.executemany("INSERT INTO blog_tag (id, name) VALUES (?, ?)", tags)
r.executemany("INSERT INTO blog_post (id, title, body, published, author_id, rating, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
              [p + (p[6],) for p in posts])
r.executemany("INSERT INTO blog_post_tags (source_id, target_id) VALUES (?, ?)", links)
r.commit()

d = sqlite3.connect(django_db)
iso = lambda ts: datetime.fromtimestamp(ts, timezone.utc).strftime("%Y-%m-%d %H:%M:%S")
d.executemany("INSERT INTO blog_author (id, name, email) VALUES (?, ?, ?)", authors)
d.executemany("INSERT INTO blog_tag (id, name) VALUES (?, ?)", tags)
d.executemany("INSERT INTO blog_post (id, title, body, published, author_id, rating, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
              [(p[0], p[1], p[2], p[3], p[4], p[5], iso(p[6]), iso(p[6])) for p in posts])
d.executemany("INSERT INTO blog_post_tags (post_id, tag_id) VALUES (?, ?)", links)
d.commit()
print(f"seeded {len(authors)} authors, {len(tags)} tags, {len(posts)} posts ({sum(p[3] for p in posts)} published), {len(links)} tag links")
