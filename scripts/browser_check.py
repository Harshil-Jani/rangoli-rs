"""Drive the admin in a real browser. Catches what HTTP-level tests can't:
htmx swaps, client-side scripts, CSS that hides content.

    python scripts/browser_check.py http://127.0.0.1:8000 admin <password>

Needs `pip install playwright && playwright install chromium` and a running app
with an empty database plus that superuser. Set BROWSER_CHANNEL=chrome to use an
installed Chrome instead of Playwright's Chromium.
"""
import os
import sys
from playwright.sync_api import sync_playwright, expect

base, user, password = sys.argv[1:4]

with sync_playwright() as p:
    page = p.chromium.launch(channel=os.environ.get("BROWSER_CHANNEL")).new_page(viewport={"width": 1280, "height": 800})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))

    page.goto(f"{base}/admin/login")
    page.fill("#id_username", user)
    page.fill("#id_password", password)
    page.click("button[type=submit]")
    expect(page).to_have_url(f"{base}/admin/")

    # Validation errors render (htmx must swap 4xx responses).
    page.goto(f"{base}/admin/blog_author/add")
    page.click("button[name=_save]")
    expect(page.locator(".field-error")).to_have_count(2)

    # Add through the UI and see Django's success message.
    for name in ["Ada Lovelace", "Alan Turing"]:
        page.goto(f"{base}/admin/blog_author/add")
        page.fill("#id_name", name)
        page.fill("#id_email", name.split()[0].lower() + "@example.com")
        page.click("button[name=_save]")
        expect(page.locator(".message.success")).to_contain_text(f"“{name}” was added successfully")

    page.goto(f"{base}/admin/blog_post/add")
    page.fill("#id_title", "Notes on the Analytical Engine")
    page.fill("#id_body", "Patterns")
    page.select_option("#id_author_id", label="Ada Lovelace")
    page.click("button[name=_save]")
    expect(page.locator(".message.success")).to_be_visible()

    # Live search updates the results and the count without a reload.
    page.goto(f"{base}/admin/blog_author/")
    page.fill("#searchbar", "alan")
    expect(page.locator("#result_list tbody tr")).to_have_count(1)
    expect(page.locator("#search-count")).to_contain_text("1 result of")

    # Selection counter (admin.js).
    page.fill("#searchbar", "")
    expect(page.locator("#result_list tbody tr")).to_have_count(2)
    page.check("#action-toggle")
    expect(page.locator(".action-counter")).to_have_text("2 of 2 selected")

    # A protected delete explains itself instead of failing silently.
    page.goto(f"{base}/admin/blog_author/")
    page.click("text=Ada Lovelace")
    page.click("a:has-text('Delete')")
    expect(page.get_by_text("protected related objects")).to_be_visible()

    # Bulk delete asks first, then deletes.
    page.goto(f"{base}/admin/blog_author/")
    page.locator("tr", has_text="Alan Turing").locator("input.action-select").check()
    page.select_option("#action-select", "delete_selected")
    page.click("button:has-text('Go')")
    expect(page.get_by_text("Are you sure you want to delete the selected")).to_be_visible()
    page.click("button:has-text('Yes, delete')")
    expect(page.locator(".message.success")).to_contain_text("Successfully deleted 1 author")

    assert not errors, errors
    print("browser check passed")
