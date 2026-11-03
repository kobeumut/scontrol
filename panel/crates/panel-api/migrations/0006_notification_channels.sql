-- Notification channels (Slack, Discord, Telegram, ntfy, Gotify, generic
-- webhook, SMTP). `config` holds only non-secret settings. The one secret
-- per channel (webhook URL, bot/app token, password, signing secret) is
-- AES-256-GCM sealed by panel-api with the channel id as associated data,
-- so a database dump does not reveal it and a ciphertext cannot be moved to
-- another row.
CREATE TABLE notification_channels (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('slack', 'discord', 'telegram', 'ntfy', 'gotify', 'webhook', 'email')),
    config JSONB NOT NULL,
    secret_sealed BYTEA,
    events TEXT[] NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    last_sent_at TIMESTAMPTZ,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_notification_channels_events
    ON notification_channels USING GIN (events) WHERE enabled;
