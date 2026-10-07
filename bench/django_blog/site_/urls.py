from django.contrib import admin
from django.urls import include, path
from rest_framework.routers import DefaultRouter

from blog import views

router = DefaultRouter()
router.register("posts", views.PostViewSet)

urlpatterns = [
    path("admin/", admin.site.urls),
    path("", views.home),
    path("posts.json", views.posts_json),
    path("hello", views.hello),
    path("api/", include(router.urls)),
]
