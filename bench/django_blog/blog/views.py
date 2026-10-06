from django.http import JsonResponse
from django.shortcuts import render
from rest_framework import serializers, viewsets
from rest_framework.pagination import LimitOffsetPagination

from .models import Post


def posts_json(request):
    """Published posts with their author's name: same shape as the Rangoli example."""
    posts = Post.objects.filter(published=True).select_related("author").order_by("-id")[:50]
    data = [
        {"id": p.id, "title": p.title, "body": p.body, "published": p.published,
         "author_id": p.author_id, "rating": p.rating, "author": p.author.name}
        for p in posts
    ]
    return JsonResponse(data, safe=False)


def home(request):
    q = request.GET.get("q", "").strip()
    posts = Post.objects.filter(published=True).select_related("author").prefetch_related("tags").order_by("-created_at")
    if q:
        from django.db.models import Q
        posts = posts.filter(Q(title__icontains=q) | Q(body__icontains=q))
    template = "home.html#posts" if request.GET.get("partial") else "home.html"
    return render(request, template, {"posts": posts[:50], "q": q})


class PostSerializer(serializers.ModelSerializer):
    class Meta:
        model = Post
        fields = ["id", "title", "body", "published", "author", "rating", "created_at", "updated_at", "tags"]


class PostViewSet(viewsets.ModelViewSet):
    queryset = Post.objects.prefetch_related("tags").order_by("id")
    serializer_class = PostSerializer
    pagination_class = LimitOffsetPagination
    permission_classes = []  # public, like Access::Public on the Rangoli side
    authentication_classes = []
