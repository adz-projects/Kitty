-- Who chooses a specialist's host.
--
-- NULL (the default, every existing row) is the daemon's own choice: the
-- `choose_host` policy, which honours a provider's subagent role, the model
-- denylist, and prefers cheap hosts. `'caller'` lets the calling model name the
-- host per call, from the panel the app armed for that one turn (a debate):
-- outside an armed turn such a specialist cannot run at all. See
-- `specialists::server::call_specialist`.
ALTER TABLE specialists ADD COLUMN host_policy TEXT;
