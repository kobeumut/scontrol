-- Durable preview lifecycle fence; never rewrite the Stage 4 migration history.
ALTER TABLE compose_projects ADD COLUMN deleting BOOLEAN NOT NULL DEFAULT FALSE;
