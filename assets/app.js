const main = document.getElementById('main');
const rawEl = document.getElementById('raw');
const srcEl = document.getElementById('src');
const gutterEl = document.getElementById('gutter');
const renEl = document.getElementById('rendered');
const mdEl = document.getElementById('md');
const statusEl = document.getElementById('status');
const findEl = document.getElementById('find');
const findQ = document.getElementById('find-q');
const findN = document.getElementById('find-n');
srcEl.readOnly = true;

function setMode(m) {
  main.className = m;
  document.querySelectorAll('.seg button').forEach(b => b.classList.toggle('on', b.dataset.mode === m));
  try { localStorage.setItem('mdp-mode', m); } catch (e) {}
  showMatch(true);
}
document.querySelectorAll('.seg button').forEach(b => b.onclick = () => setMode(b.dataset.mode));
let savedMode = ''; try { savedMode = localStorage.getItem('mdp-mode') || ''; } catch (e) {}
setMode(savedMode);

const post = msg => window.ipc.postMessage(msg);
const pickFile = () => post('open');
document.getElementById('open').onclick = pickFile;
document.getElementById('empty').onclick = pickFile;

const esc = s => s.replace(/[&<>]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c]));
const slug = s => s.trim().toLowerCase().replace(/[^\p{L}\p{N}\s-]/gu, '').replace(/\s+/g, '-');

// In-page anchors (#section) would otherwise resolve against <base> and navigate away.
document.addEventListener('click', e => {
  const a = e.target.closest('a[href^="#"]');
  if (!a) return;
  e.preventDefault();
  const t = document.getElementById(decodeURIComponent(a.getAttribute('href').slice(1)));
  if (t) t.scrollIntoView();
});

function setStatus(text, warn = false) {
  statusEl.textContent = text;
  statusEl.classList.toggle('warn', warn);
}

function showHtml(html) {
  mdEl.innerHTML = html;
  const used = new Set(Array.from(document.querySelectorAll('[id]'), el => el.id));
  const nextSuffix = new Map();
  mdEl.querySelectorAll('h1,h2,h3,h4,h5,h6').forEach(h => {
    if (h.id) return;
    const base = slug(h.textContent) || 'section';
    let id = base;
    if (used.has(id)) {
      let suffix = nextSuffix.get(base) || 1;
      do {
        id = `${base}-${suffix++}`;
      } while (used.has(id));
      nextSuffix.set(base, suffix);
    }
    h.id = id;
    used.add(id);
  });
  mdEl.querySelectorAll('pre code').forEach(el => hljs.highlightElement(el));
  markRendered();
}

// The gutter mirrors the text line by line so its numbers wrap exactly like the textarea.
// It also carries the find highlights, which show through the transparent textarea.
function syncGutter() {
  const q = query();
  gutterEl.innerHTML = srcEl.value.split('\n').map(l => {
    let html = '', at = 0;
    for (const [start, end] of matchesIn(l, q)) {
      html += esc(l.slice(at, start)) + '<mark class="hit">' + esc(l.slice(start, end)) + '</mark>';
      at = end;
    }
    return '<span class="l">' + (html + esc(l.slice(at)) || ' ') + '</span>';
  }).join('');
  showMatch(false);
}

// --- Find --------------------------------------------------------------------------------------
// Matches are highlighted in both panes; the source pane drives navigation unless it is hidden.
let findIdx = 0;
function findOpen() { return !findEl.hidden; }
function query() {
  if (!findOpen() || !findQ.value) return null;
  return new RegExp(findQ.value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'giu');
}
function srcPrimary() { return main.className !== 'ren-only'; }

function matchesIn(text, q) {
  return q ? Array.from(text.matchAll(q), match => [match.index, match.index + match[0].length]) : [];
}

function markRendered() {
  mdEl.querySelectorAll('mark.hit').forEach(m => m.replaceWith(m.textContent));
  mdEl.normalize();
  const q = query();
  if (q) {
    const w = document.createTreeWalker(mdEl, NodeFilter.SHOW_TEXT), nodes = [];
    while (w.nextNode()) nodes.push(w.currentNode);
    for (const n of nodes) {
      const t = n.nodeValue, hits = matchesIn(t, q);
      if (!hits.length) continue;
      const f = document.createDocumentFragment();
      let at = 0;
      for (const [start, end] of hits) {
        const m = document.createElement('mark');
        m.className = 'hit';
        m.textContent = t.slice(start, end);
        f.append(t.slice(at, start), m);
        at = end;
      }
      f.append(t.slice(at));
      n.replaceWith(f);
    }
  }
  showMatch(false);
}

function showMatch(scroll) {
  if (!findOpen()) return;
  const hits = (srcPrimary() ? gutterEl : mdEl).querySelectorAll('mark.hit');
  if (findIdx >= hits.length) findIdx = 0;
  findN.textContent = !findQ.value ? '' : hits.length ? `${findIdx + 1}/${hits.length}` : 'no results';
  document.querySelectorAll('mark.cur').forEach(m => m.classList.remove('cur'));
  const m = hits[findIdx];
  if (!m) return;
  m.classList.add('cur');
  if (!scroll) return;
  m.scrollIntoView({ block: 'center' });
  // Select it in the editor too, so closing the bar leaves the caret on the match.
  if (srcPrimary()) {
    const range = matchesIn(srcEl.value, query())[findIdx];
    if (range) srcEl.setSelectionRange(...range);
  }
}

function runFind() {
  // Start from the first match at or after the caret.
  const after = srcPrimary() ? matchesIn(srcEl.value, query()).findIndex(([start]) => start >= srcEl.selectionStart) : 0;
  findIdx = Math.max(after, 0);
  syncGutter();
  markRendered();
  showMatch(true);
}

function openFind() {
  if (document.body.classList.contains('is-empty')) return;
  const sel = srcEl.value.slice(srcEl.selectionStart, srcEl.selectionEnd);
  if (document.activeElement === srcEl && sel && !sel.includes('\n')) findQ.value = sel;
  findEl.hidden = false;
  findQ.focus();
  findQ.select();
  runFind();
}

function findStep(d) {
  if (!findOpen()) return openFind();
  const n = (srcPrimary() ? gutterEl : mdEl).querySelectorAll('mark.hit').length;
  if (!n) return;
  findIdx = (findIdx + d + n) % n;
  showMatch(true);
}

function closeFind() {
  findEl.hidden = true;
  syncGutter();
  markRendered();
  if (srcPrimary()) srcEl.focus({ preventScroll: true });
}

findQ.addEventListener('input', runFind);
findQ.addEventListener('keydown', e => {
  if (e.key !== 'Enter') return;
  e.preventDefault();
  findStep(e.shiftKey ? -1 : 1);
});
document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && findOpen()) { e.preventDefault(); closeFind(); }
});
document.getElementById('find-prev').onclick = () => findStep(-1);
document.getElementById('find-next').onclick = () => findStep(1);
document.getElementById('find-close').onclick = closeFind;

// --- Editing ---------------------------------------------------------------------------------
let dirty = false;
let renderTimer = 0, renderSeq = 0;
let refreshSeq = 0;
let revision = 0;
let savedRaw = null;
let pendingSave = null;
let queuedSave = null;

function setDirty(d) {
  if (d === dirty) return;
  dirty = d;
  post('dirty:' + (d ? '1' : '0'));
}

async function renderLive() {
  const seq = ++renderSeq;
  try {
    const r = await fetch('/__mdp/render', { method: 'POST', body: srcEl.value, cache: 'no-store' });
    const d = await r.json();
    if (seq !== renderSeq) return; // a newer edit already started rendering
    const keep = renEl.scrollTop;
    showHtml(d.html);
    renEl.scrollTop = keep;
  } catch (e) {
    if (seq === renderSeq) setStatus('preview error: ' + e, true);
  }
}

srcEl.addEventListener('input', () => {
  revision++;
  renderSeq++;
  syncGutter();
  setDirty(true);
  setStatus('edited');
  clearTimeout(renderTimer);
  renderTimer = setTimeout(renderLive, 120);
});

// Tab indents instead of leaving the editor. execCommand keeps native undo working.
srcEl.addEventListener('keydown', e => {
  if (e.key !== 'Tab' || e.metaKey || e.ctrlKey || e.altKey) return;
  e.preventDefault();
  if (!e.shiftKey) document.execCommand('insertText', false, '  ');
});

function editorText(raw) {
  return raw.replace(/^\uFEFF/, '').replace(/\r\n?/g, '\n');
}

function fileText(raw, text) {
  if (text === editorText(raw)) return raw;
  const bom = raw.startsWith('\uFEFF') ? '\uFEFF' : '';
  const newline = raw.match(/\r\n|[\r\n]/)?.[0] || '\n';
  return bom + text.replace(/\n/g, newline);
}

// Called from the native side (⌘S, or "Save" when closing with unsaved changes).
function save(close) {
  if (savedRaw === null) return;
  if (pendingSave) {
    if (!pendingSave.close) queuedSave = close || queuedSave === true;
    return;
  }
  if (!dirty && !close) return;

  const raw = fileText(savedRaw, srcEl.value);
  pendingSave = { raw, revision, close };
  if (close) srcEl.readOnly = true;
  post('save:' + JSON.stringify([close, savedRaw, raw]));
}

function saveFinished(ok) {
  if (!pendingSave) return;
  const { raw, revision: savedRevision, close } = pendingSave;
  const next = queuedSave;
  pendingSave = queuedSave = null;
  if (close) srcEl.readOnly = false;
  if (!ok) return;

  savedRaw = raw;
  if (savedRevision === revision) {
    setDirty(false);
    setStatus('saved ' + new Date().toLocaleTimeString());
  }
  refresh();
  if (next !== null) save(next);
}

// WebView2 keeps key presses to itself, so the menu's shortcuts never fire on Windows: handle them here.
if (navigator.userAgent.includes('Windows')) {
  const keys = {
    s: () => save(false), o: pickFile, f: openFind, g: () => findStep(1), G: () => findStep(-1),
    1: () => setMode(''), 2: () => setMode('raw-only'), 3: () => setMode('ren-only'),
  };
  document.addEventListener('keydown', e => {
    const act = keys[e.shiftKey ? e.key.toUpperCase() : e.key.toLowerCase()];
    if (!act || !e.ctrlKey || e.altKey) return;
    e.preventDefault();
    act();
  });
}
if (!navigator.userAgent.includes('Mac')) {
  const open = document.getElementById('open'), hint = document.querySelector('#empty small');
  open.title = open.title.replace('⌘', 'Ctrl+');
  document.querySelectorAll('#find button').forEach(b => b.title = b.title.replace('⌘', 'Ctrl+'));
  hint.textContent = hint.textContent.replace('⌘', 'Ctrl+');
}

// --- Loading from disk -------------------------------------------------------------------------
async function refresh() {
  const seq = ++refreshSeq;
  try {
    const d = await (await fetch('/__mdp/content', { cache: 'no-store' })).json();
    if (seq !== refreshSeq) return;
    document.body.classList.toggle('is-empty', !!d.empty);
    if (d.empty) { setStatus(''); return; }
    if (d.error) { setStatus(d.error, true); return; }
    document.querySelector('base').href = d.base;
    // Never overwrite unsaved edits with the version on disk.
    if (dirty) {
      if (d.raw !== savedRaw || d.readOnly) setStatus('file changed on disk', true);
      return;
    }
    if (!d.readOnly && d.raw === savedRaw && !statusEl.classList.contains('warn')) return;
    savedRaw = d.readOnly ? null : d.raw;
    srcEl.readOnly = !!d.readOnly;
    clearTimeout(renderTimer);
    renderSeq++;
    const keep = [rawEl.scrollTop, renEl.scrollTop];
    const text = editorText(d.raw);
    if (srcEl.value !== text) {
      const [a, b] = [srcEl.selectionStart, srcEl.selectionEnd];
      srcEl.value = text;
      srcEl.setSelectionRange(a, b);
      syncGutter();
    }
    showHtml(d.html);
    [rawEl.scrollTop, renEl.scrollTop] = keep;
    if (d.readOnly) {
      setStatus('This file is not UTF-8 and is open in read-only mode.', true);
    } else {
      setStatus('updated ' + new Date().toLocaleTimeString());
    }
  } catch (e) {
    if (seq === refreshSeq) setStatus('error: ' + e, true);
  }
}
refresh();
