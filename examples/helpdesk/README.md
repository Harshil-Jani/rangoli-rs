# Helpdesk: an AI agent on your support queue, safely

A small support desk that shows Rangoli's guardrails working together.

- Customers file tickets through the API with their own token, and each sees only their own (`owned_by`).
- Staff work tickets in the admin: replies are edited inline under each ticket, and a **Close selected tickets** action is in the menu.
- An AI agent connects from **Connect an AI agent** to triage tickets and draft replies:
  - it can't read the internal notes (a `private` field);
  - it only touches tickets and replies (its token is scoped to those tables);
  - with **Ask me before each change**, every reply it writes waits under **Pending changes** for a person.

```sh
cargo run -p helpdesk -- migrate
cargo run -p helpdesk -- createsuperuser admin
RANGOLI_DEBUG=1 cargo run -p helpdesk           # http://127.0.0.1:8000/admin/
```

Without the admin, from the command line:

```sh
cargo run -p helpdesk -- createtoken admin triage --tables desk_ticket,desk_reply --needs-approval
```

## A recorded run

- **Carol** filed "Refund please" with her token, and the ticket was recorded as hers.
- **Dave**, another customer, listed tickets and saw none.
- **Staff** added an internal note: "VIP, refund approved by finance".
- **The agent** listed its tools: 11, none of which mention the note. It read the ticket, and the note wasn't there.
- **The agent's reply** came back as `{"pending": true, "approval_id": 1}` and waited in the admin.
