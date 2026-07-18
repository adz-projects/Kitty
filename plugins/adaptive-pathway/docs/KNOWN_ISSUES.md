# Known issues

None currently tracked.

## Resolved

- **`integrations/goose/manifest.json` was stale.** Removed — nothing in this repo or in Kitty
  ever read it (Kitty registers the MCP stdio server directly into Goose's own `config.yaml`,
  see `AdaptivePathway.tsx` in the Kitty repo). The empty root-level `integrations/goose|sidecar`
  directories (a leftover skeleton from an earlier layout) were removed for the same reason. The
  real working launch mechanism is unchanged: the `adaptive-pathway-mcp` console script
  (`pyproject.toml`) or `python -m adaptive_pathway.mcp_server`.

- **`query_attribution()` couldn't be used with `decide()`'s `attribution_id`.** Fixed — `decide()`
  now records each minted `Hint`/`BlendedHint` into a bounded in-memory attribution log
  (`AdaptivePathway._attribution_log`, capped at 2000 entries) keyed by `attribution_id`.
  `query_attribution(attribution_id)` looks up the log first to resolve the real `edge_id`, then
  falls back to treating the argument as an `edge_id` directly (for callers that already have one).
