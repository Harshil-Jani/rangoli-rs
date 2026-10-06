from django.contrib import admin

from .models import Author, Post, Tag

admin.site.register(Author)
admin.site.register(Tag)


@admin.register(Post)
class PostAdmin(admin.ModelAdmin):
    list_display = ["title", "author", "published", "rating", "created_at"]
    search_fields = ["title", "body"]
    list_filter = ["published", "author", "created_at"]
    ordering = ["-created_at"]
    list_select_related = ["author"]
