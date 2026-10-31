(function (host) {
  'use strict';

  const NOW_ERR =
    'Date.now() / new Date() are unavailable in workflow scripts (breaks resume). Stamp results after the workflow returns, or pass timestamps via args.';
  const RANDOM_ERR =
    'Math.random() is unavailable in workflow scripts (breaks resume). For N independent samples, include the index in the agent label or prompt.';
  const MAX_ITEMS = 4096;
  const OPTION_KEYS = [
    'label', 'phase', 'schema', 'model', 'effort', 'isolation',
    'agentType', 'disallowedTools', 'stallMs',
  ];
  const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
  const PARAMS = [
    'args', 'agent', 'parallel', 'pipeline', 'phase', 'log', 'workflow', 'budget', 'console',
  ];

  const RealDate = Date;
  function WfDate(...parts) {
    if (!new.target || parts.length === 0) throw new Error(NOW_ERR);
    return new RealDate(...parts);
  }
  WfDate.prototype = RealDate.prototype;
  WfDate.parse = RealDate.parse;
  WfDate.UTC = RealDate.UTC;
  Object.defineProperty(WfDate, 'now', { value() { throw new Error(NOW_ERR); } });
  globalThis.Date = WfDate;
  Math.random = function random() { throw new Error(RANDOM_ERR); };

  const budget = {
    total: null,
    spent() { return host.spent(); },
    remaining() { return Infinity; },
  };

  const text = (value) => {
    if (typeof value === 'string') return value;
    try { return JSON.stringify(value); } catch (_) { return String(value); }
  };
  const message = (error) =>
    error && typeof error === 'object' && 'message' in error ? String(error.message) : String(error);

  const consoleApi = {};
  for (const name of ['log', 'info', 'debug', 'warn', 'error']) {
    consoleApi[name] = (...parts) => host.log(parts.map(text).join(' '));
  }

  function makeApi(scope) {
    let current = null;
    const groupOf = (title) => (scope.name ? `▸ ${scope.name}: ${title}` : title);

    function phase(title) {
      if (typeof title !== 'string' || title === '') {
        throw new TypeError('phase() expects a non-empty title string');
      }
      current = groupOf(title);
      host.phase(current);
    }

    function log(line) { host.log(scope.name ? `▸ ${scope.name}: ${text(line)}` : text(line)); }

    function agent(prompt, opts) {
      try {
        if (typeof prompt !== 'string') throw new TypeError('agent() expects a prompt string');
        if (opts != null && typeof opts !== 'object') {
          throw new TypeError('agent() options must be an object');
        }
        const clean = {};
        for (const key of OPTION_KEYS) {
          if (opts != null && opts[key] !== undefined && opts[key] !== null) clean[key] = opts[key];
        }
        if (clean.phase === undefined) {
          if (current !== null) clean.phase = current;
          else if (scope.name) clean.phase = `▸ ${scope.name}`;
        } else {
          clean.phase = groupOf(String(clean.phase));
        }
        return host.agent(prompt, JSON.stringify(clean)).then((reply) => JSON.parse(reply).v);
      } catch (error) {
        return Promise.reject(error);
      }
    }

    function checkItems(name, items) {
      if (items.length > MAX_ITEMS) {
        throw new Error(`${name}() accepts at most ${MAX_ITEMS} items, got ${items.length}`);
      }
    }

    async function parallel(thunks) {
      if (!Array.isArray(thunks)) throw new TypeError('parallel() expects an array of functions');
      checkItems('parallel', thunks);
      for (const thunk of thunks) {
        if (typeof thunk !== 'function') {
          throw new TypeError(
            'parallel() expects an array of functions, not promises. Wrap each call: () => agent(...)');
        }
      }
      return Promise.all(thunks.map(async (thunk, index) => {
        try {
          return await thunk();
        } catch (error) {
          host.fail(`parallel[${index}]: ${message(error)}`);
          return null;
        }
      }));
    }

    async function pipeline(items, ...stages) {
      if (!Array.isArray(items)) throw new TypeError('pipeline() expects an array as the first argument');
      checkItems('pipeline', items);
      for (const stage of stages) {
        if (typeof stage !== 'function') {
          throw new TypeError('pipeline() stages must be functions: pipeline(items, item => ..., result => ...)');
        }
      }
      return Promise.all(items.map(async (item, index) => {
        try {
          let previous = item;
          for (const stage of stages) previous = await stage(previous, item, index);
          return previous;
        } catch (error) {
          host.fail(`pipeline[${index}]: ${message(error)}`);
          return null;
        }
      }));
    }

    async function workflow(reference, childArgs) {
      if (scope.depth >= 1) {
        throw new Error('workflow() nesting is one level only: workflow() inside a child throws');
      }
      const ref = typeof reference === 'string' ? { name: reference } : reference;
      if (ref === null || typeof ref !== 'object') {
        throw new TypeError('workflow() expects a name or {scriptPath}');
      }
      const loaded = JSON.parse(host.loadWorkflow(JSON.stringify(ref)));
      const child = makeApi({ depth: scope.depth + 1, name: loaded.name });
      const run = new AsyncFunction(...PARAMS, `'use strict';\n${loaded.body}`);
      return await run(
        childArgs, child.agent, child.parallel, child.pipeline, child.phase, child.log,
        child.workflow, budget, consoleApi);
    }

    return { agent, parallel, pipeline, phase, log, workflow };
  }

  async function run(body, argsJson) {
    const args = argsJson === undefined || argsJson === 'null' ? undefined : JSON.parse(argsJson);
    const api = makeApi({ depth: 0, name: null });
    const main = new AsyncFunction(...PARAMS, `'use strict';\n${body}`);
    const result = await main(
      args, api.agent, api.parallel, api.pipeline, api.phase, api.log, api.workflow,
      budget, consoleApi);
    if (typeof result === 'function') throw new Error('workflow result cannot be a function');
    return JSON.stringify(result === undefined ? null : result);
  }

  function check(body) {
    new AsyncFunction(...PARAMS, `'use strict';\n${body}`);
  }

  return { run, check };
})
