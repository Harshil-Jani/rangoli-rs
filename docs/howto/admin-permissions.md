# Staff permissions and groups

```rust
App::new().admin_permissions()
```

With this on, superusers can still do anything, and other staff can only do what their **groups** allow. Create groups in the admin under **Groups**, with one permission per line:

```text
blog_post.view
blog_post.change
orders.*          # view, add, change and delete
*.view            # see every model, change nothing
```

Then give users their groups on the user's admin page. Every door is checked:

- **The sidebar** shows only models you can view.
- **Each permission guards its own pages:** lists, rows, History and CSV export need *view*; the add page and CSV import of new rows need *add*; saves, custom actions, undo and CSV import of existing rows need *change*; the delete page and bulk delete need *delete*.
- **Being allowed to change implies view.**

**Off by default,** so turning it on is a choice: staff with no groups will see nothing. It combines with [row-level permissions](multi-tenant.md): groups decide which models, `rows` decides which rows. [`tests/admin_perms.rs`](../../rangoli/tests/admin_perms.rs) checks each door, and fails if the check is removed.
