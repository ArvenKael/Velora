'use strict';

/* ============================================================================
   Velora console.
   Talks to: GET /api/snapshot (once), GET /api/stream (SSE), GET /api/record/{seq},
   POST /api/say.
   Rule: no server-supplied string ever reaches the DOM through innerHTML -
   everything is document.createElement + textContent, because event payloads are
   arbitrary text produced by a model and by shell commands.
   ========================================================================== */

(function () {
  /* ------------------------------------------------------------ constants -- */

  const MAX_ROWS = 2000;       // hard cap on timeline rows kept in the DOM
  const HEARTBEAT_MS = 25000;  // ping arrives ~every 10s; longer silence is "stale"
  const CHAT_SLACK = 48;       // px from the bottom that still counts as pinned
  const TL_TOP = 8;            // px from the top that counts as following the stream
  const NOTE_MS = 4500;

  const KNOWN_FAMILIES = ['operator', 'belief', 'reason', 'step', 'observation',
    'config', 'tool', 'error', 'system', 'memory', 'agent'];
  const FALLBACKS = ['--fam-x1', '--fam-x2', '--fam-x3', '--fam-x4', '--fam-x5', '--fam-x6'];

  const EVIDENCE_SHOWN = 6;    // newest evidence chips per belief; the rest are counted

  /* -------------------------------------------------------------- helpers -- */

  const $ = (id) => document.getElementById(id);

  function el(tag, cls, text) {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function clear(node) {
    while (node.firstChild) node.removeChild(node.firstChild);
  }

  /** Any JSON-ish value as display text. Never assumes a string. */
  function asText(value) {
    if (value === null || value === undefined) return '';
    if (typeof value === 'string') return value;
    if (typeof value === 'number' || typeof value === 'boolean') return String(value);
    try { return JSON.stringify(value, null, 2); } catch (_) { return String(value); }
  }

  /** The part of `kind` before the first dot, unless the server sent `family`. */
  function familyOf(record) {
    if (record && typeof record.family === 'string' && record.family) return record.family;
    const kind = record && typeof record.kind === 'string' ? record.kind : '';
    const dot = kind.indexOf('.');
    if (dot > 0) return kind.slice(0, dot);
    return kind || 'unknown';
  }

  /** Family -> CSS colour. Unknown families hash to a stable fallback colour. */
  function familyColor(family) {
    const key = String(family || 'unknown');
    if (KNOWN_FAMILIES.indexOf(key) !== -1) return 'var(--fam-' + key + ')';
    let hash = 0;
    for (let i = 0; i < key.length; i++) hash = (hash * 33 + key.charCodeAt(i)) >>> 0;
    return 'var(--' + FALLBACKS[hash % FALLBACKS.length] + ')';
  }

  function recordDate(record) {
    const secs = Number(record && record.secs);
    if (!isFinite(secs)) return null;
    const micros = Number(record && record.micros);
    return new Date(secs * 1000 + (isFinite(micros) ? Math.floor(micros / 1000) : 0));
  }

  const pad2 = (n) => (n < 10 ? '0' : '') + n;

  function clockText(date) {
    return date ? pad2(date.getHours()) + ':' + pad2(date.getMinutes()) + ':' + pad2(date.getSeconds()) : '';
  }

  /** Rows are one line tall, so fold newlines into a visible marker. */
  function oneLine(text) {
    return String(text).replace(/\s*\r?\n\s*/g, ' \u23ce ').trim();
  }

  function saysOf(record) {
    const says = record && record.says;
    if (typeof says === 'string' && says.length) return says;
    if (says !== null && says !== undefined && says !== '') return asText(says);
    return '(no summary)';
  }

  /* ------------------------------------------------------------------ dom -- */

  const dom = {
    topstats: $('topstats'),
    conn: $('conn'), connText: $('conn-text'), connMeta: $('conn-meta'),
    banner: $('banner'),
    chatScroll: $('chat-scroll'), chatList: $('chat-list'), chatEmpty: $('chat-empty'),
    chatMeta: $('chat-meta'), chatError: $('chat-error'), chatJump: $('chat-jump'),
    thinking: $('thinking'), composer: $('composer'), say: $('say'), send: $('send'),
    filters: $('filters'),
    timelineScroll: $('timeline-scroll'), timeline: $('timeline'),
    timelineEmpty: $('timeline-empty'), timelineFoot: $('timeline-foot'),
    timelineNote: $('timeline-note'), tlNew: $('tl-new'),
    beliefs: $('beliefs'), about: $('about'), stateMeta: $('state-meta')
  };

  /* ---------------------------------------------------------------- state -- */

  const state = {
    about: null,
    stats: { records: null, beliefs: null, turns_open: 0, truncated: false },
    bySeq: new Map(),        // sequence -> record
    rowBySeq: new Map(),     // sequence -> timeline <li>
    familyCounts: new Map(), // family -> rows currently in the window
    enabled: new Set(),      // empty == every family visible
    beliefs: [],
    chatSeqs: new Set(),     // sequence (or local key) already in the chat panel
    chatCount: 0,
    visibleCount: 0,
    dropped: 0,              // rows evicted by the DOM cap
    malformed: 0,            // unparseable SSE payloads
    sending: false,
    awaitingReply: false,    // a say() was accepted; hide the indicator once a reply lands
    awaitingSince: 0,
    justReplied: false,
    chatPinned: true,
    tlNewCount: 0,
    conn: 'connecting',
    lastEventAt: 0,
    streamStarted: false,
    // A message the server has accepted but history has not shown yet. Not
    // drawn as though it had happened: in this system nothing is said to have
    // happened until it is in the log.
    pendingSend: null
  };

  /* -------------------------------------------------------------- notices -- */

  const notices = new Map();  // key -> {level, text}
  const LEVEL_RANK = { info: 0, warn: 1, bad: 2 };
  const chipCounts = new Map();  // family -> count <span> in the filter bar
  let filterSignature = null;

  function setNotice(key, level, text) {
    notices.set(key, { level, text });
    renderBanner();
  }

  function clearNotice(key) {
    if (notices.delete(key)) renderBanner();
  }

  function renderBanner() {
    let worst = null;
    notices.forEach((notice) => {
      if (!worst || LEVEL_RANK[notice.level] > LEVEL_RANK[worst.level]) worst = notice;
    });
    if (!worst) {
      dom.banner.hidden = true;
      dom.banner.className = 'banner';
      clear(dom.banner);
      return;
    }
    dom.banner.hidden = false;
    dom.banner.className = 'banner ' + worst.level;
    clear(dom.banner);
    dom.banner.appendChild(el('span', 'banner-k', worst.level));
    dom.banner.appendChild(el('span', 'banner-v', worst.text));
  }

  let noteTimer = 0;
  let scrollQueued = false;
  function timelineNote(message) {
    dom.timelineNote.textContent = message;
    dom.timelineNote.hidden = false;
    if (noteTimer) clearTimeout(noteTimer);
    noteTimer = setTimeout(() => {
      dom.timelineNote.hidden = true;
      noteTimer = 0;
    }, NOTE_MS);
  }

  /* ----------------------------------------------------------- connection -- */

  function setConnection(mode, text) {
    state.conn = mode;
    dom.conn.dataset.state = mode;
    dom.connText.textContent = text;
    document.body.classList.toggle('stale', mode !== 'live' && mode !== 'connecting');
    updateConnMeta();
  }

  function updateConnMeta() {
    const ago = state.lastEventAt ? Math.round((Date.now() - state.lastEventAt) / 1000) : null;
    if (state.conn === 'live') dom.connMeta.textContent = ago === null ? '' : 'last event ' + ago + 's ago';
    else if (state.conn === 'stale') dom.connMeta.textContent = 'no heartbeat';
    else dom.connMeta.textContent = '';
  }

  function noteStreamLoss(mode, text, level, message) {
    setConnection(mode, text);
    setNotice('stream', level, message);
  }

  /* ---------------------------------------------------------------- stats -- */

  function applyStats(stats) {
    if (!stats || typeof stats !== 'object') return;
    const target = state.stats;
    if (typeof stats.records === 'number') target.records = stats.records;
    if (typeof stats.beliefs === 'number') target.beliefs = stats.beliefs;
    if (typeof stats.truncated === 'boolean') target.truncated = stats.truncated;
    if (typeof stats.turns_open === 'number') target.turns_open = stats.turns_open;
    renderTop();
    renderThinking();
    renderFoot();
    renderStateMeta();
  }

  function renderTop() {
    clear(dom.topstats);
    const stats = state.stats;
    if (typeof stats.records === 'number') dom.topstats.appendChild(statNode('records', stats.records));
    if (typeof stats.beliefs === 'number' || state.beliefs.length) {
      dom.topstats.appendChild(statNode('beliefs', state.beliefs.length || stats.beliefs));
    }
    const open = typeof stats.turns_open === 'number' ? stats.turns_open : 0;
    const turns = statNode('turns open', open);
    if (open > 0) turns.classList.add('hot');
    dom.topstats.appendChild(turns);

    const model = state.about && state.about.model;
    if (model) {
      dom.topstats.appendChild(statNode('model',
        asText(model.provider || '?') + '/' + asText(model.model || '?')));
    } else if (state.about) {
      dom.topstats.appendChild(statNode('model', 'none'));
    }
  }

  function statNode(key, value) {
    const node = el('span', 'stat');
    node.appendChild(el('span', 'stat-k', key));
    node.appendChild(el('span', 'stat-v', asText(value)));
    return node;
  }

  /* ------------------------------------------------------------- timeline -- */

  function isVisible(family) {
    return state.enabled.size === 0 || state.enabled.has(family);
  }

  function bumpFamily(family, delta) {
    const next = (state.familyCounts.get(family) || 0) + (delta || 1);
    if (next <= 0) state.familyCounts.delete(family);
    else state.familyCounts.set(family, next);
    return next;
  }

  function buildRow(record) {
    const seq = Number(record.sequence);
    const family = familyOf(record);
    const kind = asText(record.kind || 'unknown');

    const row = el('li', 'tl-row');
    row.dataset.seq = Number.isFinite(seq) ? String(seq) : '';
    row.dataset.family = family;
    row.style.setProperty('--fam', familyColor(family));
    row.tabIndex = 0;
    row.setAttribute('role', 'button');
    row.setAttribute('aria-expanded', 'false');

    const line = el('div', 'tl-line');
    line.appendChild(el('span', 'tl-seq', Number.isFinite(seq) ? '#' + seq : '#?'));

    const date = recordDate(record);
    const time = el('span', 'tl-time', clockText(date) || '--:--:--');
    if (date) time.title = date.toString();
    line.appendChild(time);

    const chip = el('span', 'tl-chip');
    chip.appendChild(el('span', 'tl-fam', family));
    chip.appendChild(el('span', 'tl-kind', kind));
    if (kind === 'reason.requested') {
      const ctx = el('span', 'tl-ctx', 'ctx');
      ctx.title = 'this record carries the full context that was shown to the model';
      chip.appendChild(ctx);
    }
    line.appendChild(chip);

    const says = saysOf(record);
    const saysNode = el('span', 'tl-says', oneLine(says));
    saysNode.title = says;
    line.appendChild(saysNode);

    row.appendChild(line);
    return row;
  }

  /** Merge a batch (oldest first) into the timeline, which reads newest first. */
  function addRecords(list, streaming) {
    if (!Array.isArray(list) || list.length === 0) return 0;

    const scroll = dom.timelineScroll;
    const atTop = scroll.scrollTop <= TL_TOP;
    const beforeHeight = scroll.scrollHeight;
    const beforeTop = scroll.scrollTop;

    let added = 0;
    for (const record of list) {
      if (!record || typeof record !== 'object') continue;
      const seq = Number(record.sequence);
      if (Number.isFinite(seq) && state.bySeq.has(seq)) continue;  // snapshot/stream overlap

      const family = familyOf(record);
      if (Number.isFinite(seq)) state.bySeq.set(seq, record);
      bumpFamily(family, 1);

      const row = buildRow(record);
      if (isVisible(family)) state.visibleCount++;
      else row.classList.add('v-hidden');
      if (Number.isFinite(seq)) state.rowBySeq.set(seq, row);
      dom.timeline.insertBefore(row, dom.timeline.firstChild);
      added++;

      // Fresh agent activity: the thinking state is honest again.
      state.justReplied = false;
    }

    if (!added) return 0;

    // enforce the DOM cap by dropping the oldest rows (the bottom of the list)
    while (dom.timeline.childElementCount > MAX_ROWS) {
      const last = dom.timeline.lastElementChild;
      if (!last) break;
      const lastFamily = last.dataset.family || 'unknown';
      if (isVisible(lastFamily)) state.visibleCount--;
      bumpFamily(lastFamily, -1);
      const lastSeq = Number(last.dataset.seq);
      if (isFinite(lastSeq) && last.dataset.seq !== '') {
        state.bySeq.delete(lastSeq);
        state.rowBySeq.delete(lastSeq);
      }
      dom.timeline.removeChild(last);
      state.dropped++;
    }

    dom.timelineEmpty.hidden = true;
    renderFilters();

    if (atTop) {
      scroll.scrollTop = 0;
    } else {
      // keep the row under the operator's eyes where it was
      scroll.scrollTop = beforeTop + (scroll.scrollHeight - beforeHeight);
      if (streaming) state.tlNewCount += added;
    }
    updateTlPill();
    renderFoot();
    renderTop();
    renderThinking();
    return added;
  }

  function updateTlPill() {
    if (state.tlNewCount > 0) {
      dom.tlNew.textContent = '\u2191 ' + state.tlNewCount + ' new';
      dom.tlNew.hidden = false;
    } else {
      dom.tlNew.hidden = true;
    }
  }

  function renderFoot() {
    const bits = [];
    if (state.visibleCount !== state.bySeq.size) {
      bits.push(state.visibleCount + ' of ' + state.bySeq.size + ' events shown');
    } else {
      bits.push(state.bySeq.size + ' events in window');
    }
    if (state.stats.records > state.bySeq.size) bits.push(state.stats.records + ' in store');
    if (state.stats.truncated) bits.push('window truncated by the server');
    if (state.dropped > 0) bits.push(state.dropped + ' oldest rows dropped from view (cap ' + MAX_ROWS + ')');
    if (state.malformed > 0) bits.push(state.malformed + ' malformed stream payload(s)');
    if (state.bySeq.size === 0) bits.length = 0;
    dom.timelineFoot.textContent = bits.join(' \u00b7 ');
  }

  function renderFilters() {
    const families = Array.from(state.familyCounts.keys()).sort();
    // The chip bar only needs rebuilding when the family set or the selection
    // changes; counts are patched in place on every batch.
    const signature = families.join(',') + '|' + Array.from(state.enabled).sort().join(',');
    if (signature === filterSignature) { updateChipCounts(); return; }
    filterSignature = signature;
    chipCounts.clear();
    clear(dom.filters);

    const all = el('button', 'chip' + (state.enabled.size === 0 ? ' on' : ''));
    all.type = 'button';
    all.appendChild(el('span', 'chip-dot'));
    all.appendChild(el('span', 'chip-name', 'all'));
    all.title = 'show every family';
    all.addEventListener('click', () => {
      state.enabled.clear();
      applyFilter();
    });
    dom.filters.appendChild(all);

    for (const family of families) {
      const button = el('button', 'chip' + (state.enabled.has(family) ? ' on' : ''));
      button.type = 'button';
      button.style.setProperty('--fam', familyColor(family));
      button.title = 'show only ' + family + ' events';
      button.appendChild(el('span', 'chip-dot'));
      button.appendChild(el('span', 'chip-name', family));
      const count = el('span', 'chip-n', state.familyCounts.get(family));
      button.appendChild(count);
      chipCounts.set(family, count);
      button.addEventListener('click', () => {
        if (state.enabled.has(family)) state.enabled.delete(family);
        else state.enabled.add(family);
        applyFilter();
      });
      dom.filters.appendChild(button);
    }
  }

  function updateChipCounts() {
    chipCounts.forEach((node, family) => {
      const next = String(state.familyCounts.get(family) || 0);
      if (node.textContent !== next) node.textContent = next;
    });
  }

  function applyFilter() {
    state.visibleCount = 0;
    state.rowBySeq.forEach((row) => {
      const record = state.bySeq.get(Number(row.dataset.seq));
      const family = record ? familyOf(record) : (row.dataset.family || 'unknown');
      const show = isVisible(family);
      row.classList.toggle('v-hidden', !show);
      if (show) state.visibleCount++;
    });
    renderFilters();
    renderFoot();
  }

  function toggleRow(row, expand) {
    const isOpen = row.classList.contains('open');
    const open = expand === true ? true : !isOpen;
    if (open === isOpen) return;
    row.classList.toggle('open', open);
    row.setAttribute('aria-expanded', open ? 'true' : 'false');
    if (!open) return;
    if (row.dataset.loaded === '1') return;   // detail already fetched; it is immutable
    row.dataset.loaded = '1';

    const stale = row.querySelector('.rec-detail');
    if (stale) row.removeChild(stale);        // e.g. a previous fetch failed
    const box = el('div', 'rec-detail');
    box.appendChild(el('div', 'detail-note', 'loading record ' + (row.dataset.seq || '?') + '\u2026'));
    box.addEventListener('click', (event) => event.stopPropagation());
    row.appendChild(box);
    loadDetail(row.dataset.seq, box);
  }

  async function loadDetail(seq, box) {
    let payload = null;
    try {
      const response = await fetch('/api/record/' + encodeURIComponent(seq), { cache: 'no-store' });
      payload = await response.json().catch(() => null);
      if (!response.ok) {
        const message = payload && payload.error ? asText(payload.error) : 'HTTP ' + response.status;
        throw new Error(message);
      }
    } catch (error) {
      clear(box);
      box.appendChild(el('div', 'detail-note bad', 'could not load record ' + seq + ': ' + error.message));
      retryDetailLater(box);   // a collapsed/expanded row should try again
      return;
    }

    clear(box);
    if (!payload || typeof payload !== 'object') {
      box.appendChild(el('div', 'detail-note bad', 'record ' + seq + ' returned an empty response'));
      retryDetailLater(box);
      return;
    }

    const record = payload.record || state.bySeq.get(Number(seq)) || null;
    if (record) {
      const meta = el('dl', 'detail-meta');
      addMeta(meta, 'sequence', '#' + asText(record.sequence));
      addMeta(meta, 'kind', record.kind);
      addMeta(meta, 'version', record.version);
      addMeta(meta, 'family', familyOf(record));
      addMeta(meta, 'sensitivity', record.sensitivity);
      const date = recordDate(record);
      if (date) addMeta(meta, 'recorded', date.toString());
      box.appendChild(meta);
    }

    if (record && record.says !== null && record.says !== undefined) {
      box.appendChild(el('div', 'detail-k', 'summary'));
      box.appendChild(el('div', 'detail-says', asText(record.says)));
    }

    const detail = payload.detail;
    if (detail !== null && detail !== undefined && asText(detail).length) {
      const isContext = record && asText(record.kind) === 'reason.requested';
      box.appendChild(el('div', 'detail-k', isContext ? 'context shown to the model' : 'detail'));
      const pre = el('pre', 'detail-text', asText(detail));
      box.appendChild(pre);
    } else {
      box.appendChild(el('div', 'detail-note', 'no further detail stored for this record'));
    }
  }

  function addMeta(list, key, value) {
    if (value === null || value === undefined || value === '') return;
    list.appendChild(el('dt', null, key));
    list.appendChild(el('dd', null, asText(value)));
  }

  /** A failed fetch must not mark the row permanently loaded. */
  function retryDetailLater(box) {
    const row = box.parentNode;
    if (row && row.dataset) row.dataset.loaded = '0';
  }

  /** Jump from a belief's evidence (or `since`) to the record in the timeline. */
  function focusRecord(seq) {
    const row = state.rowBySeq.get(seq);
    const record = state.bySeq.get(seq);
    if (!row || !record) {
      timelineNote('record #' + seq + ' is not in the loaded window - it is still in the store');
      return;
    }
    const family = familyOf(record);
    if (!isVisible(family)) {
      state.enabled.add(family);   // never leave a requested record hidden
      applyFilter();
    }
    if (!row.classList.contains('open')) toggleRow(row, true);
    row.scrollIntoView({ block: 'center', behavior: 'smooth' });
    row.classList.remove('flash');
    void row.offsetWidth;          // restart the animation
    row.classList.add('flash');
    setTimeout(() => row.classList.remove('flash'), 1700);
  }

  /* ----------------------------------------------------------------- chat -- */

  function chatKey(line) {
    // Number(null) is 0, which is finite, which is how a line with no sequence
    // at all came to be drawn as "#0". Only a real number counts.
    const seq = typeof (line && line.sequence) === 'number' ? line.sequence : null;
    return seq !== null && Number.isFinite(seq) ? 's' + seq : null;
  }

  function buildMessage(line) {
    const fromSystem = asText(line && line.who) === 'velora';
    const text = asText(line && line.text);

    const item = el('li', 'msg ' + (fromSystem ? 'from-agent' : 'from-operator'));

    const meta = el('div', 'msg-meta');
    const seq = typeof (line && line.sequence) === 'number' ? line.sequence : null;
    if (seq !== null && Number.isFinite(seq)) meta.appendChild(el('span', 'msg-seq', '#' + seq));
    const date = lineDate(line);
    if (date) meta.appendChild(el('span', 'msg-time', clockText(date)));
    meta.appendChild(el('span', 'msg-who', fromSystem ? 'velora' : 'operator'));
    item.appendChild(meta);

    item.appendChild(el('div', 'msg-body', text || '(empty)'));
    return item;
  }

  /** When a line was said, from the record it came from. */
  function lineDate(line) {
    const seq = typeof (line && line.sequence) === 'number' ? line.sequence : null;
    if (seq === null) return null;

    const record = state.bySeq.get(seq);
    if (!record) return null;
    const secs = Number(record.secs);
    if (!Number.isFinite(secs)) return null;
    const micros = Number(record.micros);
    return new Date(secs * 1000 + (Number.isFinite(micros) ? micros / 1000 : 0));
  }

  function appendChat(line) {
    // Something said is in history, so whatever was said about it not being
    // there is no longer true.
    if (asText(line && line.who) === 'operator' && state.pendingSend) {
      state.pendingSend = null;
      clearChatError();
    }

    const key = chatKey(line);
    if (key !== null && state.chatSeqs.has(key)) return false;
    if (key !== null) state.chatSeqs.add(key);

    dom.chatList.appendChild(buildMessage(line));
    state.chatCount++;
    dom.chatEmpty.hidden = true;
    renderChatMeta();

    if (asText(line && line.who) === 'velora') {
      state.justReplied = true;
      state.sending = false;
      state.awaitingReply = false;
      renderThinking();
    } else {
      state.justReplied = false;
    }

    if (state.chatPinned) scrollChatToEnd();
    else updateChatJump();
    return true;
  }

  function setDialogue(list) {
    clear(dom.chatList);
    state.chatSeqs.clear();
    state.chatCount = 0;
    const lines = Array.isArray(list) ? list : [];
    for (const line of lines) appendChat(line);
    dom.chatEmpty.hidden = lines.length > 0;
    state.chatPinned = true;
    scrollChatToEnd();
    renderChatMeta();
    renderThinking();
  }

  function renderChatMeta() {
    dom.chatMeta.textContent = state.chatCount === 0 ? 'empty' : state.chatCount + ' messages';
  }

  function scrollChatToEnd() {
    state.chatPinned = true;
    if (scrollQueued) return;
    scrollQueued = true;
    requestAnimationFrame(() => {
      scrollQueued = false;
      dom.chatScroll.scrollTop = dom.chatScroll.scrollHeight;
      updateChatJump();
    });
  }

  function updateChatJump() {
    const scroll = dom.chatScroll;
    const overflows = scroll.scrollHeight > scroll.clientHeight + CHAT_SLACK;
    dom.chatJump.hidden = state.chatPinned || !overflows;
  }

  function renderThinking() {
    const open = typeof state.stats.turns_open === 'number' ? state.stats.turns_open : 0;
    // turns_open is authoritative; awaitingReply bridges the gap between the POST
    // being accepted and the server reporting the open turn, and gives up after 20s
    // so a crashed turn cannot leave the console claiming to think forever.
    const awaiting = state.awaitingReply && (open > 0 || Date.now() - state.awaitingSince < 20000);
    const show = state.sending || awaiting || (open > 0 && !state.justReplied);
    dom.thinking.hidden = !show;
    dom.thinking.textContent = state.sending ? 'sending\u2026' : 'Velora is thinking';
    dom.send.disabled = state.sending;
    if (show && state.chatPinned) scrollChatToEnd();
  }

  function showChatError(message) {
    dom.chatError.textContent = message;
    dom.chatError.hidden = false;
  }

  function clearChatError() {
    dom.chatError.hidden = true;
    dom.chatError.textContent = '';
  }

  function autoSize() {
    dom.say.style.height = 'auto';
    dom.say.style.height = Math.min(dom.say.scrollHeight, 144) + 'px';
  }

  async function sendMessage() {
    const text = dom.say.value;
    if (!text.trim() || state.sending) return;

    state.sending = true;

    // Armed *before* the request, not after it. The record can reach the stream
    // before the response reaches this function — the server polls history every
    // 150ms and the round trip is not instantaneous — and arming it afterwards
    // left the flag set with nothing left to clear it, which showed up as a
    // warning that the message had never been recorded when it had.
    state.pendingSend = { at: Date.now() };

    clearChatError();
    renderThinking();

    let response = null;
    let payload = null;
    try {
      response = await fetch('/api/say', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ text })
      });
      payload = await response.json().catch(() => null);
      if (!response.ok) {
        const message = payload && payload.error ? asText(payload.error) : 'HTTP ' + response.status;
        throw new Error(message);
      }
    } catch (error) {
      // The operator's text stays in the box: never lose what they typed.
      showChatError('could not send: ' + error.message + ' \u2014 your text is still in the box');
      state.pendingSend = null;
      state.sending = false;
      state.awaitingReply = false;
      renderThinking();
      return;
    }

    dom.say.value = '';
    autoSize();

    // The message is not drawn here. The server cannot say what its sequence is
    // — the loop assigns that when it appends — so anything drawn now would be a
    // guess wearing a sequence number, and when the real record arrived a
    // moment later it would appear twice. The record reaches this panel through
    // the stream within a poll interval, so the operator sees it almost at once,
    // and what they see is what actually happened.

    state.sending = false;
    state.awaitingReply = true;
    state.awaitingSince = Date.now();
    state.chatPinned = true;
    scrollChatToEnd();
    renderThinking();
    dom.say.focus();
  }

  /* ------------------------------------------------------------- beliefs -- */

  function confidencePercent(value) {
    if (typeof value === 'number' && isFinite(value)) return value <= 1 ? value * 100 : value;
    const parsed = parseFloat(asText(value).replace('%', ''));
    return isFinite(parsed) ? parsed : null;
  }

  function setBeliefs(list) {
    state.beliefs = Array.isArray(list) ? list : [];
    state.stats.beliefs = state.beliefs.length;
    renderBeliefs();
    renderTop();
    renderStateMeta();
  }

  function renderBeliefs() {
    clear(dom.beliefs);
    if (state.beliefs.length === 0) {
      dom.beliefs.appendChild(el('p', 'empty',
        'No beliefs yet. Velora has not concluded anything about this machine - ' +
        'beliefs appear here as it forms them, with the evidence they rest on.'));
      return;
    }

    for (const belief of state.beliefs) {
      if (!belief || typeof belief !== 'object') continue;
      const card = el('div', 'belief' + (belief.contested ? ' contested' : ''));

      const head = el('div', 'belief-head');
      head.appendChild(el('span', 'belief-key', asText(belief.key) || '(unnamed)'));
      head.appendChild(el('span', 'belief-conf', asText(belief.confidence) || '\u2014'));
      if (belief.contested) head.appendChild(el('span', 'belief-contested', 'contested'));
      card.appendChild(head);

      card.appendChild(el('div', 'belief-value', asText(belief.value)));

      const percent = confidencePercent(belief.confidence);
      const bar = el('div', 'conf-bar');
      const fill = el('div', 'conf-fill');
      fill.style.width = (percent === null ? 0 : Math.max(0, Math.min(100, percent))) + '%';
      bar.appendChild(fill);
      card.appendChild(bar);

      if (belief.rationale !== null && belief.rationale !== undefined && asText(belief.rationale)) {
        card.appendChild(el('div', 'belief-k', 'rationale'));
        card.appendChild(el('div', 'belief-rationale', asText(belief.rationale)));
      }

      const evidence = Array.isArray(belief.evidence) ? belief.evidence : [];
      if (evidence.length || typeof belief.since === 'number') {
        const row = el('div', 'belief-ev');
        row.appendChild(el('span', 'belief-k', 'evidence'));
        if (typeof belief.since === 'number') {
          row.appendChild(evidenceChip(belief.since, 'since'));
        }

        // A belief gathers evidence for as long as it is reinforced, so this
        // list grows without bound: rendering all of it buries every other
        // belief under a wall of numbers. The newest are shown and the rest are
        // counted, so what the panel says about how much is behind a belief is
        // still true.
        const total = typeof belief.evidence_total === 'number'
          ? belief.evidence_total : evidence.length;
        const shown = evidence.slice(-EVIDENCE_SHOWN);
        const earlier = total - shown.length;
        if (earlier > 0) {
          row.appendChild(el('span', 'ev-more', '+' + earlier + ' earlier'));
        }
        for (const item of shown) row.appendChild(evidenceChip(item, null));
        card.appendChild(row);
      }

      dom.beliefs.appendChild(card);
    }
  }

  function evidenceChip(value, label) {
    const seq = Number(value);
    const text = '#' + (isFinite(seq) ? seq : asText(value));
    const button = el('button', 'ev-chip', label ? label + ' ' + text : text);
    button.type = 'button';
    if (isFinite(seq)) {
      button.title = 'show record ' + text + ' in the timeline';
      button.addEventListener('click', () => focusRecord(seq));
    } else {
      button.disabled = true;
    }
    return button;
  }

  /* --------------------------------------------------------------- about -- */

  function renderAbout() {
    clear(dom.about);
    const about = state.about;
    if (!about || typeof about !== 'object') {
      dom.about.appendChild(el('p', 'empty',
        'No runtime details reported yet - they arrive with /api/snapshot on load.'));
      return;
    }

    if (about.system !== undefined) aboutRow(dom.about, 'system', asText(about.system));
    if (about.store !== undefined) aboutRow(dom.about, 'store', asText(about.store));

    const durability = el('div', 'durability');
    durability.appendChild(el('span', 'durability-k', 'durability'));
    durability.appendChild(el('span', 'durability-v', asText(about.durability) || 'not reported'));
    dom.about.appendChild(durability);

    const model = about.model;
    if (model && typeof model === 'object') {
      aboutRow(dom.about, 'model', asText(model.provider || '?') + ' / ' + asText(model.model || '?'));
      const flags = el('div', 'chips');
      flags.appendChild(tag('streaming', model.streaming === true));
      flags.appendChild(tag('tools', model.tools === true));
      dom.about.appendChild(flags);
    } else {
      dom.about.appendChild(el('div', 'no-model',
        'no model configured - Velora is running without a language model; reasoning is unavailable.'));
    }

    const pipeline = Array.isArray(about.pipeline) ? about.pipeline : [];
    if (pipeline.length) {
      const block = el('div', 'about-block');
      block.appendChild(el('div', 'about-k', 'pipeline'));
      const list = el('ol', 'pipeline');
      for (const stage of pipeline) {
        const item = el('li');
        item.appendChild(el('span', 'stage-name', asText(stage)));
        list.appendChild(item);
      }
      block.appendChild(list);
      dom.about.appendChild(block);
    }

    const procedures = Array.isArray(about.procedures) ? about.procedures : [];
    if (procedures.length) {
      const block = el('div', 'about-block');
      block.appendChild(el('div', 'about-k', 'procedures the agent can run'));
      const chips = el('div', 'chips');
      for (const procedure of procedures) chips.appendChild(el('span', 'tag', asText(procedure)));
      block.appendChild(chips);
      dom.about.appendChild(block);
    }

    if (about.persona !== null && about.persona !== undefined && asText(about.persona)) {
      const details = el('details', 'persona');
      details.appendChild(el('summary', null, 'persona'));
      details.appendChild(el('pre', null, asText(about.persona)));
      dom.about.appendChild(details);
    }
  }

  function aboutRow(parent, key, value) {
    const row = el('div', 'about-row');
    row.appendChild(el('span', 'about-k', key));
    row.appendChild(el('span', 'about-v', value || '\u2014'));
    parent.appendChild(row);
  }

  function tag(label, on) {
    return el('span', 'tag ' + (on ? 'on' : 'off'), label + (on ? '' : ' off'));
  }

  function renderStateMeta() {
    const beliefs = state.beliefs.length;
    const records = state.stats.records;
    const bits = [beliefs + (beliefs === 1 ? ' belief' : ' beliefs')];
    if (typeof records === 'number') bits.push(records + ' events');
    dom.stateMeta.textContent = bits.join(' \u00b7 ');
  }

  /* -------------------------------------------------------------- network -- */

  function applySnapshot(data) {
    if (!data || typeof data !== 'object') return;

    state.about = data.about || null;

    // reset the timeline window
    clear(dom.timeline);
    state.bySeq.clear();
    state.rowBySeq.clear();
    state.familyCounts.clear();
    state.visibleCount = 0;
    state.dropped = 0;
    state.tlNewCount = 0;
    dom.timelineEmpty.hidden = true;

    setDialogue(data.dialogue);
    addRecords(data.records, false);
    if (!data.records || data.records.length === 0) dom.timelineEmpty.hidden = false;

    setBeliefs(data.beliefs);
    applyStats(data.stats);
    renderAbout();
    renderFilters();
    renderFoot();
    renderChatMeta();
    renderStateMeta();
    renderTop();
  }

  async function loadSnapshot() {
    try {
      const response = await fetch('/api/snapshot', { cache: 'no-store' });
      if (!response.ok) throw new Error('HTTP ' + response.status);
      const data = await response.json();
      applySnapshot(data);
      clearNotice('snapshot');
      startStream();
    } catch (error) {
      setNotice('snapshot', 'bad', 'could not read /api/snapshot (' + error.message + ') - retrying every 3s');
      setTimeout(loadSnapshot, 3000);
    }
  }

  /** After a reconnect, fill the gap without throwing away what is on screen. */
  async function resync() {
    try {
      const response = await fetch('/api/snapshot', { cache: 'no-store' });
      if (!response.ok) return;
      const data = await response.json();
      if (data.about) { state.about = data.about; renderAbout(); renderTop(); }
      let highest = -1;
      state.bySeq.forEach((_record, seq) => { if (seq > highest) highest = seq; });
      const missing = (Array.isArray(data.records) ? data.records : [])
        .filter((record) => Number(record && record.sequence) > highest);
      if (missing.length) addRecords(missing, true);
      if (Array.isArray(data.beliefs)) setBeliefs(data.beliefs);
      if (Array.isArray(data.dialogue)) {
        for (const line of data.dialogue) appendChat(line);
      }
      applyStats(data.stats);
    } catch (_) {
      /* the stream is already reconnecting; the banner stays up */
    }
  }

  function parsePayload(event) {
    try {
      return JSON.parse(event.data);
    } catch (error) {
      state.malformed++;
      console.warn('velora: ignoring malformed "' + event.type + '" stream payload', event.data);
      renderFoot();
      return null;
    }
  }

  function startStream() {
    if (state.streamStarted || typeof EventSource === 'undefined') return;
    state.streamStarted = true;

    const source = new EventSource('/api/stream');
    let opened = false;

    source.addEventListener('open', () => {
      state.lastEventAt = Date.now();
      setConnection('live', 'live');
      clearNotice('stream');
      if (opened) resync();       // reconnect: refill anything missed while down
      opened = true;
    });

    source.addEventListener('records', (event) => {
      state.lastEventAt = Date.now();
      const payload = parsePayload(event);
      if (!payload) return;
      if (payload.stats) applyStats(payload.stats);
      // What was said arrives with the records that said it: a line of dialogue
      // is a reading of two ordinary kinds of record, and the server does the
      // reading so the screen and the model cannot disagree about it.
      if (Array.isArray(payload.dialogue)) {
        for (const line of payload.dialogue) appendChat(line);
      }
      addRecords(payload.records, true);
    });

    source.addEventListener('beliefs', (event) => {
      state.lastEventAt = Date.now();
      const payload = parsePayload(event);
      if (!payload) return;
      // Only a full list may replace the panel; a partial payload must not blank it.
      if (Array.isArray(payload.beliefs)) setBeliefs(payload.beliefs);
      else {
        state.malformed++;
        console.warn('velora: beliefs event carried no belief list, keeping the current one');
        renderFoot();
      }
    });

    source.addEventListener('ping', (event) => {
      state.lastEventAt = Date.now();
      const payload = parsePayload(event);
      if (payload) applyStats(payload);
      if (state.conn !== 'live') setConnection('live', 'live');
      clearNotice('stream');
    });

    source.addEventListener('error', () => {
      if (source.readyState === EventSource.CLOSED) {
        noteStreamLoss('closed', 'disconnected', 'bad',
          'the event stream is closed - showing the last known state; reload the page to retry');
      } else {
        noteStreamLoss('reconnecting', 'reconnecting', 'warn',
          'event stream dropped - reconnecting automatically, showing the last known state');
      }
    });
  }

  /* --------------------------------------------------------------- wiring -- */

  function init() {
    dom.composer.addEventListener('submit', (event) => {
      event.preventDefault();
      sendMessage();
    });

    dom.say.addEventListener('keydown', (event) => {
      if (event.key !== 'Enter' || event.shiftKey || event.isComposing) return;
      event.preventDefault();
      sendMessage();
    });

    dom.say.addEventListener('input', autoSize);

    dom.chatScroll.addEventListener('scroll', () => {
      const scroll = dom.chatScroll;
      state.chatPinned = scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight <= CHAT_SLACK;
      updateChatJump();
    });

    dom.chatJump.addEventListener('click', scrollChatToEnd);

    dom.timelineScroll.addEventListener('scroll', () => {
      if (dom.timelineScroll.scrollTop <= TL_TOP) {
        state.tlNewCount = 0;
        updateTlPill();
      }
    });

    dom.tlNew.addEventListener('click', () => {
      state.tlNewCount = 0;
      updateTlPill();
      dom.timelineScroll.scrollTo({ top: 0, behavior: 'smooth' });
    });

    dom.timeline.addEventListener('click', (event) => {
      const row = event.target.closest('.tl-row');
      if (!row) return;
      if (String(window.getSelection())) return;   // a text selection is not a click
      toggleRow(row);
    });

    dom.timeline.addEventListener('keydown', (event) => {
      if (event.key !== 'Enter' && event.key !== ' ') return;
      const row = event.target.closest('.tl-row');
      if (!row) return;
      event.preventDefault();
      toggleRow(row);
    });

    // heartbeat watchdog: the server pings ~every 10s
    setInterval(() => {
      updateConnMeta();

      // Accepted but never recorded: the loop took the message and has not
      // written it. Saying nothing would look exactly like a message that was
      // ignored, which is the one thing this screen exists to make impossible.
      if (state.pendingSend && Date.now() - state.pendingSend.at > 5000) {
        showChatError('your message was accepted, but it is not in the history yet \u2014 ' +
          'the loop may be stopped. It is not lost: it will be recorded when the loop runs.');
      }

      if (state.conn === 'live' && state.lastEventAt &&
          Date.now() - state.lastEventAt > HEARTBEAT_MS) {
        noteStreamLoss('stale', 'stale', 'warn',
          'no heartbeat from the server for ' + Math.round((Date.now() - state.lastEventAt) / 1000) +
          's - the view below may be out of date');
      }
    }, 2000);

    setConnection('connecting', 'connecting');
    renderAbout();
    renderBeliefs();
    renderFilters();
    renderFoot();
    renderChatMeta();
    renderStateMeta();
    renderTop();
    renderThinking();
    autoSize();
    loadSnapshot();
  }

  init();
})();
