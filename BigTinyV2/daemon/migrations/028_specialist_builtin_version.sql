-- Which release of a built-in specialist's definition a row holds.
--
-- Built-ins used to be seeded with `ON CONFLICT DO NOTHING`, so an install kept
-- whatever definition it was first seeded with forever: a sharper prompt, the
-- shared preamble, a corrected tool list never reached it. A built-in row is
-- only ever written by seeding -- editing one through the API is refused, and
-- "editing a built-in" makes the app its own shadowing copy -- so replacing a
-- stale one loses nothing a user wrote. The version lets seeding replace only
-- rows older than the definition it carries, and leave newer ones (from a
-- later engine sharing the database) alone.
--
-- 0 for every existing row, which is older than any real version, so the next
-- start refreshes them all once.

ALTER TABLE specialists ADD COLUMN builtin_version INTEGER NOT NULL DEFAULT 0;
