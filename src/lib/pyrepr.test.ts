import { describe, it, expect } from 'vitest';
import { parsePyRepr, tryParsePyRepr } from './pyrepr';

/** Backs Kitty's need to read hint data out of the Adaptive Pathway
    extension's MCP tool output, which is `str(python_dict)` — repr syntax,
    not JSON (Round-C Batch 1). */

describe('parsePyRepr', () => {
  it('parses the real _format_result shape, including an apostrophe in hint text', () => {
    const input =
      "{'hints': [{'text': \"don't do this\", 'confidence': 0.8, 'type': 'single', " +
      "'primitive': 'X', 'domain': 'd', 'attribution_id': 'abc-1', 'edge_id': 'edge-1'}], " +
      "'confidence': 0.7, 'novelty': 0.2, 'is_flow_state': False}";
    expect(parsePyRepr(input)).toEqual({
      hints: [
        {
          text: "don't do this",
          confidence: 0.8,
          type: 'single',
          primitive: 'X',
          domain: 'd',
          attribution_id: 'abc-1',
          edge_id: 'edge-1',
        },
      ],
      confidence: 0.7,
      novelty: 0.2,
      is_flow_state: false,
    });
  });

  it('parses nested dicts and lists', () => {
    const input = "{'a': {'b': [1, 2, {'c': 3}]}, 'd': []}";
    expect(parsePyRepr(input)).toEqual({ a: { b: [1, 2, { c: 3 }] }, d: [] });
  });

  it('parses None as null', () => {
    expect(parsePyRepr("{'x': None}")).toEqual({ x: null });
  });

  it('parses negative and scientific-notation floats', () => {
    expect(parsePyRepr("{'a': -0.5, 'b': -1, 'c': 1e-3}")).toEqual({ a: -0.5, b: -1, c: 0.001 });
  });

  it('parses empty dicts and lists', () => {
    expect(parsePyRepr('{}')).toEqual({});
    expect(parsePyRepr('[]')).toEqual([]);
  });

  it('parses a bare tuple as an array', () => {
    expect(parsePyRepr('(1, 2, 3)')).toEqual([1, 2, 3]);
  });

  it('handles double-quoted strings and escaped characters', () => {
    expect(parsePyRepr('{"key": "line1\\nline2"}')).toEqual({ key: 'line1\nline2' });
  });

  it('throws on malformed input', () => {
    expect(() => parsePyRepr("{'a': ")).toThrow();
    expect(() => parsePyRepr("{'a': 1 'b': 2}")).toThrow();
  });
});

describe('tryParsePyRepr', () => {
  it('returns the parsed value on success', () => {
    expect(tryParsePyRepr("{'ok': True}")).toEqual({ ok: true });
  });

  it('returns null instead of throwing on malformed input', () => {
    expect(tryParsePyRepr('not a dict at all {{{')).toBeNull();
  });
});
