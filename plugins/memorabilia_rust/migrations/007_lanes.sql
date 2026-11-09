-- Phase 4.1: two intake lanes. `current`: something recent the chat model
-- would not know (a release, decision, status, figure, event) -- what brings
-- it up to date, decays fast, may be superseded. `durable`: explanation,
-- history, reasoned argument -- slow to age. The paragraph screen says which
-- lane kept a passage; a document is `current` when any of its kept
-- paragraphs came in through that lane. The chunk carries its document's
-- lane so extraction never files current material as `static`.
ALTER TABLE captures ADD COLUMN lane TEXT NOT NULL DEFAULT 'durable';
ALTER TABLE chunks ADD COLUMN lane TEXT NOT NULL DEFAULT 'durable';
