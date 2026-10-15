# Sending email, and "Forgot your password?"

```sh
export RANGOLI_SMTP_URL=smtp://apikey:SECRET@smtp.sendgrid.net:587     # STARTTLS; smtps://...:465 for TLS from the start
export RANGOLI_EMAIL_FROM="Acme <no-reply@acme.com>"
```

```rust
rangoli::mail::send(Mail::new("bob@example.com", "Your order shipped", "Track it here: ...").html("<p>...</p>")).await?;
```

- **`send` queues the email;** the task queue delivers it, retrying with backoff if the server is down. It works with any SMTP service (SES, Postmark, SendGrid, Mailgun, your own).
- **Without `RANGOLI_SMTP_URL`, emails print to the console,** so development needs no mail server.
- **Encryption is required:** to any host but this machine, Rangoli refuses to send credentials or mail unencrypted.
- **No header injection:** line breaks in addresses or subjects are refused, so user input can't add headers.

## Password reset

Give users an email address in the admin, and the login page gets a **Forgot your password?** link:

- **The same answer for every address,** so nobody can test which accounts exist.
- **At most one email per account every 2 minutes.**
- **Links last an hour and work once.** Only a hash is stored.
- **Setting the new password signs out every session** of the account.

For your own front end, call `auth::request_password_reset(email, |token| format!("https://app.example.com/reset/{token}"))`, then `auth::reset_password(token, new_password)`.

Tested against a real SMTP server (Mailpit) in CI. The e2e test reads the reset link out of the queued email.
