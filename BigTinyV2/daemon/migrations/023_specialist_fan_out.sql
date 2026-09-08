-- Whether one `call_specialist` call with N refs becomes N delegates.
--
-- `'per_ref'` or NULL. This is the fan-out the caller *cannot* express by
-- batching its own tool calls: `execute_tools` already runs a step's tool calls
-- concurrently, so a model that emits three `call_specialist` calls in one step
-- already gets three parallel delegates — but it can only do that when it knows
-- how many there are, and the usual case is a folder or a search result rather
-- than a count.
--
-- Declared per specialist rather than passed per call because the split is a
-- property of the work, not of the request: extracting fields from ten documents
-- is ten independent jobs, while *locating* something across ten documents is
-- one job that happens to read ten files. Handing that judgment to the calling
-- model is exactly the kind of decision small models get wrong.

ALTER TABLE specialists ADD COLUMN fan_out TEXT;

-- The two built-ins whose schemas already return arrays, and whose work really
-- is per-document.
UPDATE specialists SET fan_out = 'per_ref'
WHERE app_id IS NULL AND name IN ('extractor', 'summarizer');
