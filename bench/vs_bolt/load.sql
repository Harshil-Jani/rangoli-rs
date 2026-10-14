-- Copy django-bolt's seeded benchmark rows into Rangoli's tables, ids and all.
-- Rangoli stores datetimes as unix seconds.
TRUNCATE rb_comment, rb_article_tags, rb_article, rb_tag, rb_author, rb_user RESTART IDENTITY CASCADE;
INSERT INTO rb_user (id, username, email, first_name, last_name, is_active)
  SELECT id, username, email, first_name, last_name, is_active FROM benchmark_users;
INSERT INTO rb_author (id, name, email, bio, created_at)
  SELECT id, name, email, bio, extract(epoch FROM created_at)::bigint FROM articles_author;
INSERT INTO rb_tag (id, name, slug) SELECT id, name, slug FROM articles_tag;
INSERT INTO rb_article (id, title, content, published, author_id, created_at, updated_at)
  SELECT id, title, content, published, author_id, extract(epoch FROM created_at)::bigint, extract(epoch FROM updated_at)::bigint
  FROM articles_article;
INSERT INTO rb_article_tags (source_id, target_id) SELECT article_id, tag_id FROM articles_article_tags;
INSERT INTO rb_comment (id, article_id, author_name, content, created_at)
  SELECT id, article_id, author_name, content, extract(epoch FROM created_at)::bigint FROM articles_comment;
SELECT setval(pg_get_serial_sequence(t, 'id'), (SELECT coalesce(max(id), 1) FROM rb_article))
  FROM unnest(ARRAY['rb_article']) t;
