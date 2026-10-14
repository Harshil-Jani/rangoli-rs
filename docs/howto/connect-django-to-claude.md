# Connect Claude to your Django database in five minutes

You have a Django app and want Claude (or Cursor) to answer questions about its data. You don't want to give a model a database connection, or rewrite anything.

## 1. Start Rangoli on the Django database

```sh
git clone https://github.com/Harshil-Jani/rangoli-rs && cd rangoli-rs
RANGOLI_DATABASE_URL=postgres://user:pw@host/your_django_db cargo run -p rangoli-django
```

Rangoli reads the tables from the database itself; there's nothing to declare. MySQL and SQLite URLs work too.

- **Left out:** Django's own tables (`auth_*`, `django_*`).
- **Hidden from agents:** columns whose names look secret (`password`, `api_key`, `token`, `secret`, ...). Staff still see them in the admin.
- **Added:** Rangoli creates its own `rangoli_*` tables and never alters Django's.

## 2. Sign in with your Django account

Open http://127.0.0.1:8000/admin/ and sign in with a Django **staff** username and password; Django's password hashes are checked directly. You'll see every table of your app, searchable, read-only by default.

## 3. Connect the agent

Click **Connect an AI agent**:

1. Name it ("Claude on my laptop").
2. Leave **Allow changes** off.
3. Optionally tick the tables it may see.
4. Copy the line for your client, for example:

```sh
claude mcp add --transport http rangoli http://127.0.0.1:8000/mcp --header "Authorization: Bearer rgl_..."
```

**claude.ai or Claude Desktop:** in Settings, under Connectors, choose **Add custom connector** and paste `https://your-server/mcp`. Claude sends you to a Rangoli page to sign in and pick read only, ask-first or read-and-change. Claude needs to reach the server, so for a laptop, put it behind a tunnel (for example `cloudflared tunnel --url http://127.0.0.1:8000`) and use that https address.

Then ask: *"Which authors published the most posts last month?"* Claude calls tools like `blog_post_list` with filters, sorting and pagination.

## What the agent can't do

- **Write anything:** read-only is enforced by the server, not just left out of the tool list.
- **See private columns,** even through filters or sorting.
- **Open the admin,** or touch tables outside its scope.
- **Run away:** it gets 120 requests a minute, then 429s.

To let it change data, see [Let an AI agent change data, safely](safe-agent-writes.md). To let staff edit Django's data through Rangoli, start it with `RANGOLI_DJANGO_WRITES=1`.
