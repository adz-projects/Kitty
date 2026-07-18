/** Parses Python `repr()` syntax — single/double-quoted strings, `True`/
    `False`/`None`, `{}` dicts, `[]` lists, `()` tuples — into plain JS values.

    Needed because the Adaptive Pathway extension's MCP tools return
    `str(some_dict)` (Python repr), not JSON, so `JSON.parse` can't be used.
    A naive quote-swap (`'` → `"`) would break on any string containing an
    apostrophe (e.g. hint text like "don't do this again"), which is exactly
    why this is a small real parser instead of a regex replace. */

class PyReprParseError extends Error {}

function skipWs(s: string, i: number): number {
  while (i < s.length && /\s/.test(s[i])) i++;
  return i;
}

function parseString(s: string, i: number): [string, number] {
  const quote = s[i];
  i++;
  let out = '';
  const escapes: Record<string, string> = {
    n: '\n',
    t: '\t',
    r: '\r',
    '\\': '\\',
    "'": "'",
    '"': '"',
  };
  while (i < s.length && s[i] !== quote) {
    if (s[i] === '\\' && i + 1 < s.length) {
      out += escapes[s[i + 1]] ?? s[i + 1];
      i += 2;
    } else {
      out += s[i];
      i++;
    }
  }
  if (s[i] !== quote) throw new PyReprParseError('unterminated string');
  return [out, i + 1];
}

function parseNumber(s: string, i: number): [number, number] {
  const start = i;
  if (s[i] === '-' || s[i] === '+') i++;
  while (i < s.length && /[0-9.eE+-]/.test(s[i])) i++;
  const token = s.slice(start, i);
  const n = Number(token);
  if (Number.isNaN(n)) throw new PyReprParseError(`invalid number: ${token}`);
  return [n, i];
}

function parseCollection(s: string, i: number, close: string): [unknown[], number] {
  i++; // opening bracket
  const out: unknown[] = [];
  i = skipWs(s, i);
  if (s[i] === close) return [out, i + 1];
  for (;;) {
    let value: unknown;
    [value, i] = parseValue(s, i);
    out.push(value);
    i = skipWs(s, i);
    if (s[i] === ',') {
      i++;
      i = skipWs(s, i);
      if (s[i] === close) {
        i++;
        break;
      }
      continue;
    }
    if (s[i] === close) {
      i++;
      break;
    }
    throw new PyReprParseError(`expected ',' or '${close}' at index ${i}`);
  }
  return [out, i];
}

function parseDict(s: string, i: number): [Record<string, unknown>, number] {
  i++; // '{'
  const out: Record<string, unknown> = {};
  i = skipWs(s, i);
  if (s[i] === '}') return [out, i + 1];
  for (;;) {
    let key: unknown;
    [key, i] = parseValue(s, i);
    i = skipWs(s, i);
    if (s[i] !== ':') throw new PyReprParseError(`expected ':' at index ${i}`);
    i++;
    let value: unknown;
    [value, i] = parseValue(s, i);
    out[String(key)] = value;
    i = skipWs(s, i);
    if (s[i] === ',') {
      i++;
      i = skipWs(s, i);
      if (s[i] === '}') {
        i++;
        break;
      }
      continue;
    }
    if (s[i] === '}') {
      i++;
      break;
    }
    throw new PyReprParseError(`expected ',' or '}' at index ${i}`);
  }
  return [out, i];
}

function parseValue(s: string, i: number): [unknown, number] {
  i = skipWs(s, i);
  const c = s[i];
  if (c === undefined) throw new PyReprParseError('unexpected end of input');
  if (c === '{') return parseDict(s, i);
  if (c === '[') return parseCollection(s, i, ']');
  if (c === '(') return parseCollection(s, i, ')');
  if (c === "'" || c === '"') return parseString(s, i);
  if (s.startsWith('True', i)) return [true, i + 4];
  if (s.startsWith('False', i)) return [false, i + 5];
  if (s.startsWith('None', i)) return [null, i + 4];
  return parseNumber(s, i);
}

/** Parses a full Python-repr string. Throws on malformed/trailing input. */
export function parsePyRepr(input: string): unknown {
  const [value, end] = parseValue(input, 0);
  const rest = input.slice(end).trim();
  if (rest.length > 0) {
    throw new PyReprParseError(`unexpected trailing input at ${end}: ${rest.slice(0, 30)}`);
  }
  return value;
}

/** Safe wrapper for UI call sites — a malformed/unexpected tool-call output
    should never crash a render. */
export function tryParsePyRepr(input: string): unknown | null {
  try {
    return parsePyRepr(input);
  } catch {
    return null;
  }
}
