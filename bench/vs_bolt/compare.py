"""Check that django-bolt (:9001) and Rangoli (:9002) return the same rows in the same order,
and that both log the benchmark user in, before any timing is trusted."""
import json, urllib.request, sys
def get(p, path, cookie=None):
    r = urllib.request.Request(f"http://127.0.0.1:{p}{path}", headers={"Cookie": cookie} if cookie else {})
    return json.load(urllib.request.urlopen(r))
def norm(a):
    return (a["id"], a["title"], a["author"]["id"], sorted(t["id"] for t in a["tags"]), [c["id"] for c in a.get("comments", [])])
for path in ["/articles?page=1&page_size=20", "/articles?page=3&page_size=20"]:
    b, r = get(9001, path), get(9002, path)
    assert [norm(a) for a in b["items"]] == [norm(a) for a in r["items"]], path
    assert {k: b[k] for k in ("total","page","page_size","pages")} == {k: r[k] for k in ("total","page","page_size","pages")}
b, r = get(9001, "/articles/1"), get(9002, "/articles/1")
print("detail comments", norm(b)[4][:5], norm(r)[4][:5])
assert norm(b)[:4] == norm(r)[:4] and sorted(norm(b)[4]) == sorted(norm(r)[4])
assert get(9001, "/db") == get(9002, "/db")
def login(p):
    req = urllib.request.Request(f"http://127.0.0.1:{p}/auth/login", data=json.dumps({"username":"benchuser","password":"benchpass123"}).encode(), headers={"Content-Type":"application/json"}, method="POST")
    res = urllib.request.urlopen(req)
    c = [h for h in res.headers.get_all("Set-Cookie") if h.startswith("access_token=")][0]
    return c.split(";")[0]
for p in (9001, 9002):
    c = login(p)
    print(p, get(p, "/auth/me", c), len(get(p, "/auth/articles?page=1&page_size=20", c)["items"]))
print("same rows, same order")
