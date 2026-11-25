-- Opt-in: attach every container of the service to its environment's
-- internal Docker network (`scontrol-env-<environment id>`), where database
-- services of the same environment answer by their slug. Database services
-- always join; ordinary services opt in because Docker also registers each
-- service name on the network, and two projects defining the same service
-- name there could resolve each other.
ALTER TABLE compose_projects
    ADD COLUMN join_shared_network BOOLEAN NOT NULL DEFAULT FALSE;
