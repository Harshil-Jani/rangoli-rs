# Let an AI agent change data, safely

Reading is easy to allow. Writing is where agents do damage: a prompt injection in a support ticket, a loop, or a confident wrong answer. Rangoli gives you four controls, all enforced by the server.

## 1. Scope the token

In **Connect an AI agent**, tick **Allow changes**, then tick only the tables it needs. From the command line:

```sh
cargo run -- createtoken alice triage --tables desk_ticket,desk_reply --needs-approval
```

Tables outside the scope answer 404, as if they didn't exist, and their tools aren't listed.

## 2. Ask me first

Leave **Ask me before each change** on (it is by default once changes are allowed). Every create, update or delete the agent sends is checked as usual and then stored under **Pending changes**; the agent gets back:

```json
{"pending": true, "approval_id": 12, "detail": "Waiting for a person to approve this in the admin (Pending changes)."}
```

Staff select changes and run **Approve selected changes** or **Reject selected changes**. An approved change runs as the user who asked, with permissions checked again, and History says `Added via MCP, approved by alice.` The agent can check with the `approval_id` it got:

```json
{"name": "approval_status", "arguments": {"approval_id": 12}}
```

## 3. Keep secrets out of reach

```rust
#[field(text, private)]
pub internal_note: Option<String>,   // never in the API or MCP; staff see it in the admin

#[field(encrypted)]
pub tax_id: String,                  // encrypted in the database, too
```

## 4. Undo

Every change or delete made through the API or MCP stores the row as it was. Open the row's **History**: the newest agent change has an **Undo** button.

- An undone add is removed.
- An undone change is put back.
- An undone delete returns with the same id.

## Why this holds up against prompt injection

The test suite replays the 2025 Supabase MCP attack: a support ticket tells the agent to copy secrets into a new message. A fully obedient agent still fails at every step:

- the secret is a private field;
- other tables are outside its scope;
- it can't write anything out;
- it can't reach the admin.

Models still get fooled; here, being fooled doesn't leak anything.
