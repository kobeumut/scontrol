-- Git push webhooks for Compose projects.
--
-- Only a random nonce is stored. The webhook secret is derived on demand as
-- HMAC-SHA256(server key, project id + nonce), so the database never holds
-- it; rotating the nonce revokes the old secret. NULL disables the webhook.
ALTER TABLE compose_projects ADD COLUMN webhook_nonce TEXT;
