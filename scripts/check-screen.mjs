/**
 * Checks that the screen renders, without a browser.
 *
 * A headless browser is not available in every environment this is developed in
 * — it needs somewhere to write a profile and a display to capture — and "the
 * page is served" is not the same claim as "the page works". So this runs the
 * real `index.html` and the real `app.js` against a running system, with a small
 * DOM implementation standing in for the browser, and asserts that the screen
 * ends up saying what the system is actually doing.
 *
 * It is not a browser, and it does not pretend to be one: it has no layout, no
 * rendering and no CSS. What it does have is the thing a browser would exercise
 * — the code path from a real snapshot to real DOM calls — which is where a
 * mistake would actually be.
 *
 *   cargo run -p velora-bin &          # with the screen on
 *   node scripts/check-screen.mjs
 *   node scripts/check-screen.mjs --url http://127.0.0.1:7799
 */

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const assets = join(here, '..', 'crates', 'velora-web', 'assets');

const urlFlag = process.argv.indexOf('--url');
const base = urlFlag === -1 ? 'http://127.0.0.1:7799' : process.argv[urlFlag + 1];

/* ------------------------------------------------------------------- DOM -- */

const listeners = new WeakMap();
const parent = new WeakMap();

function classes(node) {
  if (!node.__classes) {
    node.__classes = new Set();
  }
  return node.__classes;
}

function makeNode(tag) {
  const node = {
    tagName: String(tag).toUpperCase(),
    children: [],
    attrs: {},
    dataset: {},
    style: { setProperty() {}, removeProperty() {} },
    __text: '',
    __classes: new Set(),
    __handlers: {},
    hidden: false,
    value: '',
    scrollTop: 0,
    scrollHeight: 0,
    clientHeight: 0,
    clientWidth: 0,
    offsetWidth: 0,
    disabled: false,
    title: '',
    href: '',
    type: '',
    placeholder: '',
    rows: 0,
  };

  Object.defineProperty(node, 'className', {
    get: () => [...classes(node)].join(' '),
    set: (value) => {
      node.__classes = new Set(String(value).split(/\s+/).filter(Boolean));
    },
  });

  Object.defineProperty(node, 'classList', {
    get: () => ({
      add: (...names) => names.forEach((name) => classes(node).add(name)),
      remove: (...names) => names.forEach((name) => classes(node).delete(name)),
      toggle: (name, force) => {
        const on = force === undefined ? !classes(node).has(name) : Boolean(force);
        if (on) classes(node).add(name);
        else classes(node).delete(name);
        return on;
      },
      contains: (name) => classes(node).has(name),
    }),
  });

  Object.defineProperty(node, 'textContent', {
    get: () =>
      node.__text + node.children.map((child) => child.textContent).join(''),
    set: (value) => {
      node.children.length = 0;
      node.__text = value === null || value === undefined ? '' : String(value);
    },
  });

  Object.defineProperty(node, 'firstChild', {
    get: () => node.children[0] ?? null,
  });

  Object.defineProperty(node, 'parentNode', { get: () => parent.get(node) ?? null });

  node.appendChild = (child) => {
    parent.set(child, node);
    node.children.push(child);
    return child;
  };
  node.append = (...kids) => kids.forEach(node.appendChild);
  node.prepend = (child) => {
    parent.set(child, node);
    node.children.unshift(child);
    return child;
  };
  node.insertBefore = (child, before) => {
    const at = node.children.indexOf(before);
    parent.set(child, node);
    if (at === -1) node.children.push(child);
    else node.children.splice(at, 0, child);
    return child;
  };
  node.removeChild = (child) => {
    const at = node.children.indexOf(child);
    if (at !== -1) node.children.splice(at, 1);
    return child;
  };
  node.remove = () => node.parentNode?.removeChild(node);
  node.replaceChildren = (...kids) => {
    node.children.length = 0;
    kids.forEach(node.appendChild);
  };
  node.setAttribute = (name, value) => {
    node.attrs[name] = String(value);
    if (name === 'class') node.className = value;
    if (name === 'id') node.id = String(value);
  };
  node.getAttribute = (name) => (name in node.attrs ? node.attrs[name] : null);
  node.removeAttribute = (name) => {
    delete node.attrs[name];
  };
  node.focus = () => {};
  node.blur = () => {};
  node.scrollIntoView = () => {};
  node.cloneNode = () => makeNode(tag);
  node.contains = () => false;

  node.addEventListener = (type, handler) => {
    (node.__handlers[type] ||= []).push(handler);
  };
  node.removeEventListener = (type, handler) => {
    const held = node.__handlers[type];
    if (held) node.__handlers[type] = held.filter((each) => each !== handler);
  };
  /** Fires handlers directly, so the script can act like a person. */
  node.fire = (type, event = {}) => {
    const held = node.__handlers[type] || [];
    const shaped = {
      preventDefault() {},
      stopPropagation() {},
      target: node,
      key: '',
      shiftKey: false,
      ...event,
    };
    for (const handler of held) handler(shaped);
    return held.length;
  };

  node.closest = (selector) => {
    let at = node;
    while (at) {
      if (node.matches(at, selector)) return at;
      at = parent.get(at) ?? null;
    }
    return null;
  };
  node.matches = (selector) => false;
  node.querySelector = (selector) => find(node, selector);
  node.querySelectorAll = (selector) => findAll(node, selector);
  node.matches = (selector) => false;

  Object.defineProperty(node, 'children', { value: node.children, writable: false });

  return node;
}

/** The `matches` used by `closest`, kept out of the node to avoid recursion. */
function matches(node, selector) {
  if (selector.startsWith('.')) return classes(node).has(selector.slice(1));
  if (selector.startsWith('#')) return node.id === selector.slice(1);
  return node.tagName === selector.toUpperCase();
}

function walk(node, visit) {
  for (const child of node.children) {
    visit(child);
    walk(child, visit);
  }
}

function find(root, selector) {
  let found = null;
  walk(root, (node) => {
    if (found === null && matches(node, selector)) found = node;
  });
  return found;
}

function findAll(root, selector) {
  const found = [];
  walk(root, (node) => {
    if (matches(node, selector)) found.push(node);
  });
  return found;
}

/* --------------------------------------------------------------- document -- */

const html = readFileSync(join(assets, 'index.html'), 'utf8');
const body = makeNode('body');
const byId = new Map();

for (const match of html.matchAll(/id="([^"]+)"/g)) {
  const node = makeNode('div');
  node.id = match[1];
  byId.set(match[1], node);
  body.appendChild(node);
}

const document = {
  body,
  documentElement: makeNode('html'),
  getElementById: (id) => byId.get(id) ?? null,
  createElement: (tag) => makeNode(tag),
  createTextNode: (text) => {
    const node = makeNode('#text');
    node.textContent = text;
    return node;
  },
  querySelector: (selector) => find(body, selector),
  querySelectorAll: (selector) => findAll(body, selector),
  addEventListener: () => {},
  readyState: 'complete',
};

const errors = [];

const window = {
  document,
  location: { href: base + '/', origin: base, reload() {} },
  requestAnimationFrame: (callback) => setTimeout(callback, 0),
  cancelAnimationFrame: (id) => clearTimeout(id),
  setTimeout,
  clearTimeout,
  setInterval,
  clearInterval,
  fetch: (input, init) =>
    fetch(String(input).startsWith('http') ? input : base + input, init),
  EventSource: class {
    constructor() {
      this.readyState = 0;
    }
    addEventListener() {}
    close() {}
  },
  addEventListener: () => {},
  onerror: null,
};

/* ------------------------------------------------------------------ run -- */

process.on('unhandledRejection', (reason) => errors.push('unhandled rejection: ' + reason));
process.on('uncaughtException', (error) => errors.push('uncaught: ' + error.message));

const app = readFileSync(join(assets, 'app.js'), 'utf8');

// The script is an IIFE over the globals a browser provides, so it is run the
// way a browser would run it: through the function constructor, with those
// globals in scope.
const run = new Function('window', 'document', 'fetch', 'EventSource', 'setTimeout',
  'clearTimeout', 'setInterval', 'clearInterval', 'requestAnimationFrame', app);

run(window, document, window.fetch, window.EventSource, setTimeout, clearTimeout,
  setInterval, clearInterval, window.requestAnimationFrame);

// Let the snapshot request and the render that follows it finish.
await new Promise((resolve) => setTimeout(resolve, 2500));

/* --------------------------------------------------------------- checks -- */

const text = body.textContent;
let failures = 0;

function check(what, ok, detail = '') {
  if (ok) {
    console.log('  ok   ' + what);
  } else {
    failures += 1;
    console.log('  FAIL ' + what + (detail ? ' — ' + detail : ''));
  }
}

const snapshot = await (await fetch(base + '/api/snapshot')).json();

console.log('\nrendering ' + base + '\n');

check('the script threw nothing', errors.length === 0, errors.join('; '));
const timeline = byId.get('timeline');
check('the timeline rendered records', timeline.children.length > 0,
  'rows=' + timeline.children.length);

// The rows carry the sequences they came from, newest first. Checked against
// each other rather than against a second snapshot, because the system is live
// and a second snapshot is a different history.
const seqs = timeline.children.map((row) =>
  row.dataset.seq === undefined ? null : Number(row.dataset.seq),
);
check('every row carries the sequence it came from', seqs.every((each) => each !== null));
check(
  'the timeline runs newest first',
  seqs.every((each, at) => at === 0 || seqs[at - 1] > each),
  seqs.slice(0, 5).join(' > '),
);
check(
  'the rows are records the server actually has',
  seqs.every((each) => each === null || each <= snapshot.records[snapshot.records.length - 1].sequence),
  'newest row #' + seqs[0] + ', history at #' + snapshot.records[snapshot.records.length - 1].sequence,
);
check('the system name is on the page', text.includes(snapshot.about.system));
check(
  'the durability promise is shown',
  text.includes(snapshot.about.durability),
  snapshot.about.durability,
);
check('the store is shown', text.includes(snapshot.about.store));

for (const belief of snapshot.beliefs.slice(0, 3)) {
  check('belief shown: ' + belief.key, text.includes(belief.key));
}

for (const line of snapshot.dialogue.slice(-4)) {
  const head = line.text.slice(0, 40);
  check('said on screen (' + line.who + '): ' + JSON.stringify(head), text.includes(head));
}

for (const procedure of snapshot.about.procedures.slice(0, 3)) {
  check('procedure listed: ' + procedure, text.includes(procedure));
}

// The one interaction a person cannot do without: typing something and sending
// it. Fired the way a browser fires it, against the real endpoint.
const input = byId.get('say');
const before = (await (await fetch(base + '/api/snapshot')).json()).stats.records;
if (input) {
  input.value = 'a message from the check script';
  input.fire('input');
  const sent = input.fire('keydown', { key: 'Enter' });
  await new Promise((resolve) => setTimeout(resolve, 2500));
  const after = (await (await fetch(base + '/api/snapshot')).json()).stats.records;
  check('enter in the box submits a message', sent > 0 && after > before,
    `handlers=${sent} records ${before} -> ${after}`);
} else {
  check('the message box exists', false, 'no textarea found');
}

console.log('');
if (failures > 0) {
  console.log(failures + ' check(s) failed\n');
  process.exit(1);
}
console.log('the screen renders, and the system answers it\n');
process.exit(0);
