-- Authenticator secret that claims a guild like the rescue key does.
ALTER TABLE rescue_key ADD COLUMN totp_secret TEXT;
