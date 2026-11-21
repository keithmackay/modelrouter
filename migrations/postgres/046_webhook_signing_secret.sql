-- Secret a webhook's deliveries are signed with (HMAC-SHA256 over
-- "<unix timestamp>.<raw body>"). NULL leaves the webhook unsigned.
ALTER TABLE webhook_callbacks ADD COLUMN IF NOT EXISTS signing_secret TEXT;
