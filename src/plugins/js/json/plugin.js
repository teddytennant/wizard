/// <reference path="../../../../docs/wizard-plugin.d.ts" />

// `json_query`: pull one value out of a JSON document without reading the rest.
//
// The first plugin written in JavaScript, and it is here because it is the
// shape that argues for the backend rather than the shape that merely fits in
// it. `docs/plugins.md` records what a scripted plugin may not be — no
// session state, nothing core calls synchronously, no seams a value cannot
// carry — and this has none of those problems. What decides *JavaScript* over
// Lua is narrower and more specific:
//
// **JSON is JavaScript's value model, and it is not Lua's.** Lua has one table
// type, so `[]` and `{}` are the same value and a serializer has to guess.
// `src/kernel/lua/host.rs` carries an `object_schema` repair for the half of
// that it can fix and a note saying the other half is unfixable: "a plugin
// that genuinely needs an empty JSON array somewhere still cannot write one."
// A tool whose entire job is to read a JSON document, select part of it, and
// hand that part back *unchanged* cannot be written on top of a value model
// that rewrites `{"tags": []}` into `{"tags": {}}` on the way through. In
// JavaScript there is nothing to get right: `JSON.parse` and `JSON.stringify`
// are the round trip, and `src/kernel/js/convert.rs` preserves both shapes
// across the host boundary.
//
// The second reason is smaller and still real: the query walk below is about
// forty lines because objects, arrays and `undefined` are three different
// things here. The Lua version needs a table-kind heuristic in every branch.
//
// What this must not become is a second implementation of anything. The byte
// budgets are `wizard.limits`, the head/tail framing and the spill file are
// `wizard.truncate`, and the file read is `wizard.fs.read`, which is confined
// to the project directory because this plugin declares no `filesystem`
// capability and does not need one.

/** How much of a document is worth reading into a VM at all.
 *
 * Not a budget on the *answer* — that is `wizard.limits` — but on the input.
 * `JSON.parse` of a 200 MB file inside a 64 MB VM is an out-of-memory abort
 * with no useful message, and the honest failure is a sentence naming the
 * size. Ten megabytes is far past any config file and far below the ceiling.
 */
const MAX_DOCUMENT_BYTES = 10 * 1024 * 1024;

/** How many matches a wildcard query may return before the answer is a count.
 *
 * A `[*]` over a large array is the easy way to accidentally ask for the whole
 * document back, one element at a time. The cap is generous for the case the
 * tool is for — "show me every dependency's version" — and turns the runaway
 * case into a number plus advice rather than a truncated list the model reads
 * as complete.
 */
const MAX_MATCHES = 500;

/**
 * Split a query into path segments.
 *
 * The syntax is the one people already type into a debugger: `a.b`, `a[0]`,
 * `a[*]`, `a.*`. Quoted brackets (`a["b.c"]`) are how a key containing a dot
 * is named, because otherwise there is no way to reach one and real documents
 * have them.
 *
 * An empty query is the whole document, which is what `json_query` with no
 * query should mean — "parse this and show me" is a useful thing to ask.
 *
 * @param {string} query
 * @returns {Array<string | number | "*">}
 */
function parseQuery(query) {
  const segments = [];
  let rest = query.trim();
  if (rest.startsWith("$")) rest = rest.slice(1);
  if (rest.startsWith(".")) rest = rest.slice(1);

  let index = 0;
  let current = "";
  const flush = () => {
    if (current !== "") {
      segments.push(current === "*" ? "*" : current);
      current = "";
    }
  };

  while (index < rest.length) {
    const ch = rest[index];
    if (ch === ".") {
      flush();
      index += 1;
    } else if (ch === "[") {
      flush();
      const close = rest.indexOf("]", index);
      if (close === -1) {
        throw new Error(`json_query: unclosed '[' in query '${query}'`);
      }
      let inner = rest.slice(index + 1, close).trim();
      if (
        (inner.startsWith('"') && inner.endsWith('"')) ||
        (inner.startsWith("'") && inner.endsWith("'"))
      ) {
        segments.push(inner.slice(1, -1));
      } else if (inner === "*") {
        segments.push("*");
      } else if (/^-?\d+$/.test(inner)) {
        segments.push(Number(inner));
      } else {
        throw new Error(
          `json_query: '[${inner}]' is not an index, a '*', or a quoted key`,
        );
      }
      index = close + 1;
    } else {
      current += ch;
      index += 1;
    }
  }
  flush();
  return segments;
}

/**
 * Walk `segments` from `root` and collect everything they reach.
 *
 * Breadth-first over a frontier rather than recursion, so a `*` in the middle
 * of a path fans out and the rest of the path applies to each branch. A
 * segment that matches nothing narrows the frontier to empty and the caller
 * reports that, rather than throwing: "no match" is an answer about the
 * document, and the model can act on it.
 *
 * A negative index counts from the end, which is what everyone tries first.
 *
 * @param {unknown} root
 * @param {Array<string | number | "*">} segments
 * @returns {unknown[]}
 */
function select(root, segments) {
  let frontier = [root];
  for (const segment of segments) {
    const next = [];
    for (const node of frontier) {
      if (node === null || node === undefined) continue;
      if (segment === "*") {
        if (Array.isArray(node)) {
          next.push(...node);
        } else if (typeof node === "object") {
          next.push(...Object.values(node));
        }
        continue;
      }
      if (typeof segment === "number") {
        if (!Array.isArray(node)) continue;
        const index = segment < 0 ? node.length + segment : segment;
        if (index >= 0 && index < node.length) next.push(node[index]);
        continue;
      }
      if (typeof node === "object" && !Array.isArray(node)) {
        // `hasOwn` rather than `in`, so a document with a key called
        // "constructor" or "toString" answers about itself instead of about
        // Object.prototype.
        if (Object.hasOwn(node, segment)) next.push(node[segment]);
      }
    }
    frontier = next;
    if (frontier.length > MAX_MATCHES) break;
  }
  return frontier;
}

/**
 * One line describing a value's shape, for the summary above a long answer.
 *
 * @param {unknown} value
 * @returns {string}
 */
function describe(value) {
  if (value === null) return "null";
  if (Array.isArray(value)) return `array of ${value.length}`;
  const kind = typeof value;
  if (kind === "object") return `object with ${Object.keys(value).length} keys`;
  return kind;
}

/**
 * Read the document this call is about, from a path or from inline text.
 *
 * @param {{ path?: string, text?: string }} args
 * @returns {unknown}
 */
function load(args) {
  if (typeof args.text === "string") {
    if (args.text.length > MAX_DOCUMENT_BYTES) {
      throw new Error(
        `json_query: text is ${args.text.length} bytes; the limit is ${MAX_DOCUMENT_BYTES}`,
      );
    }
    return JSON.parse(args.text);
  }
  if (typeof args.path !== "string" || args.path === "") {
    throw new Error("json_query: give either 'path' or 'text'");
  }
  const raw = wizard.fs.read(args.path);
  if (raw.length > MAX_DOCUMENT_BYTES) {
    throw new Error(
      `json_query: ${args.path} is ${raw.length} bytes; the limit is ${MAX_DOCUMENT_BYTES}. ` +
        "Narrow it with a shell tool first.",
    );
  }
  return JSON.parse(raw);
}

export default {
  name: "json",

  apply(ctx) {
    ctx.tool({
      name: "json_query",
      description:
        "Read a JSON file (or inline JSON text) and return just the part a query selects, " +
        "instead of the whole document. Query syntax: dots for keys, [n] for array indices " +
        "(negative counts from the end), [\"key.with.dots\"] for awkward keys, and * or [*] " +
        "to match every element or value at that level. An empty query returns the whole " +
        "document, pretty-printed.",
      parameters: {
        type: "object",
        properties: {
          path: {
            type: "string",
            description:
              "Path to a JSON file, relative to the project directory.",
          },
          text: {
            type: "string",
            description:
              "JSON to query directly, instead of reading a file. Wins over 'path' if both are given.",
          },
          query: {
            type: "string",
            description:
              "What to select, e.g. 'dependencies', 'items[0].name', 'scripts.*', 'files[-1]'.",
          },
        },
      },
      access: "read_only",
      execute(args) {
        args = args || {};
        let document;
        try {
          document = load(args);
        } catch (err) {
          // A bad path, a document that is not JSON, or one too large to
          // parse. All three are news about the input rather than a broken
          // tool, so they come back as a soft failure with the reason and the
          // model gets to try something else.
          return { content: String(err && err.message ? err.message : err), is_error: true };
        }

        const query = typeof args.query === "string" ? args.query : "";
        let segments;
        try {
          segments = parseQuery(query);
        } catch (err) {
          return { content: String(err.message), is_error: true };
        }

        if (segments.length === 0) {
          return {
            content: wizard.truncate(
              JSON.stringify(document, null, 2),
              wizard.limits.output,
            ),
          };
        }

        const matches = select(document, segments);
        if (matches.length === 0) {
          // Deliberately not an error. The document parsed and the query
          // parsed; what happened is that the key is not there, which is a
          // fact the model asked for.
          return `No match for '${query}'.`;
        }
        if (matches.length > MAX_MATCHES) {
          return {
            content:
              `'${query}' matches more than ${MAX_MATCHES} values. ` +
              "Narrow the query, or index into one branch at a time.",
            is_error: true,
          };
        }

        // One match answers as itself; several answer as an array, so the
        // reply is still valid JSON either way and the model can feed it
        // straight back in.
        const answer = matches.length === 1 ? matches[0] : matches;
        const rendered = JSON.stringify(answer, null, 2);
        // `undefined` has no JSON spelling, so `stringify` returns undefined
        // for it rather than a string. It reaches here only when a key exists
        // and holds nothing.
        if (rendered === undefined) {
          return `'${query}' is present and holds no value.`;
        }
        const header =
          matches.length === 1
            ? `${query} (${describe(answer)})`
            : `${query} — ${matches.length} matches`;
        return {
          content: wizard.truncate(
            `${header}\n${rendered}`,
            wizard.limits.output,
          ),
        };
      },
    });
  },
};
