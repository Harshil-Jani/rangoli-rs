"""django-bolt endpoints: Rust (Actix) HTTP server, Python handlers, the Django ORM."""
from django_bolt import BoltAPI

from .models import Post

api = BoltAPI()


@api.get("/hello")
def hello(request) -> dict:
    return {"message": "Hello, World!"}


@api.get("/posts.json")
def posts_json(request) -> list:
    posts = Post.objects.filter(published=True).select_related("author").order_by("-id")[:50]
    return [
        {"id": p.id, "title": p.title, "body": p.body, "published": p.published,
         "author_id": p.author_id, "rating": p.rating, "author": p.author.name}
        for p in posts
    ]


@api.get("/hello-async")
async def hello_async(request) -> dict:
    return {"message": "Hello, World!"}


@api.get("/posts-async.json")
async def posts_json_async(request) -> list:
    posts = Post.objects.filter(published=True).select_related("author").order_by("-id")[:50]
    return [
        {"id": p.id, "title": p.title, "body": p.body, "published": p.published,
         "author_id": p.author_id, "rating": p.rating, "author": p.author.name}
        async for p in posts
    ]
