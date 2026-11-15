'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (s, root = document) => root.querySelector(s);
const $$ = (s, root = document) => [...root.querySelectorAll(s)];
const el = (tag, cls, text) => {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
};
const icon = (name) => `<svg><use href="#i-${name}"/></svg>`;

// ---------- Форматирование ----------
const UNITS = ['Б', 'КБ', 'МБ', 'ГБ', 'ТБ'];
function fmtBytes(n) {
  if (!n || n < 0) return '0 Б';
  let i = 0;
  while (n >= 1024 && i < UNITS.length - 1) { n /= 1024; i++; }
  return `${n >= 100 || i === 0 ? n.toFixed(0) : n >= 10 ? n.toFixed(1) : n.toFixed(2)} ${UNITS[i]}`;
}
const fmtSpeed = (mbps) => `${fmtBytes((mbps || 0) * 1048576)}/с`;
function fmtEta(secs) {
  if (secs == null || !isFinite(secs)) return '';
  if (secs < 60) return `${Math.max(1, Math.round(secs))} с`;
  if (secs < 3600) return `${Math.floor(secs / 60)} мин ${Math.round(secs % 60)} с`;
  if (secs < 86400) return `${Math.floor(secs / 3600)} ч ${Math.floor((secs % 3600) / 60)} мин`;
  return `${Math.floor(secs / 86400)} д ${Math.floor((secs % 86400) / 3600)} ч`;
}

// ---------- Состояние ----------
const state = {
  torrents: [],
  settings: null,
  filter: 'all',
  query: '',
  selected: null,
  tab: 'files',
  details: null, // { id, files }
  history: [],
};
const rows = new Map(); // id -> элемент

const STATUS = {
  downloading: 'Загрузка',
  seeding: 'Раздаётся',
  done: 'Завершено',
  paused: 'Пауза',
  checking: 'Проверка',
  error: 'Ошибка',
};

function statusOf(t) {
  const s = t.stats || {};
  if (s.state === 'error') return 'error';
  if (s.state === 'initializing') return 'checking';
  if (s.state === 'paused') return s.finished ? 'done' : 'paused';
  return s.finished ? 'seeding' : 'downloading';
}
const isPaused = (t) => t.stats?.state === 'paused' || (t.stats?.state === 'initializing' && t.stats?.initializing_paused);

function matches(t, f) {
  const st = statusOf(t);
  switch (f) {
    case 'downloading': return st === 'downloading' || st === 'checking';
    case 'seeding': return st === 'seeding';
    case 'done': return !!t.stats?.finished;
    case 'paused': return st === 'paused';
    default: return true;
  }
}

function toast(msg, bad) {
  const t = el('div', 'toast' + (bad ? ' bad' : ''), String(msg));
  $('#toasts').append(t);
  setTimeout(() => t.remove(), bad ? 7000 : 3500);
}
const fail = (err) => toast(err, true);

// ---------- Список ----------
function makeRow(t) {
  const row = el('div', 't');
  row.dataset.id = t.id;
  row.innerHTML = `
    <div class="t-name"></div>
    <div class="t-actions">
      <button class="icon a-toggle"></button>
      <button class="icon a-folder" title="Открыть папку">${icon('folder')}</button>
      <button class="icon danger a-remove" title="Удалить">${icon('trash')}</button>
    </div>
    <div class="bar"><i></i></div>
    <div class="t-meta"></div>`;
  row.addEventListener('click', () => select(state.selected === t.id ? null : t.id));
  $('.a-toggle', row).addEventListener('click', (ev) => { ev.stopPropagation(); toggle(t.id); });
  $('.a-folder', row).addEventListener('click', (ev) => { ev.stopPropagation(); invoke('open_folder', { id: t.id }).catch(fail); });
  $('.a-remove', row).addEventListener('click', (ev) => { ev.stopPropagation(); askRemove(t.id); });
  return row;
}

function updateRow(row, t) {
  const s = t.stats || {};
  const st = statusOf(t);
  const live = s.live;
  const pct = s.total_bytes ? (s.progress_bytes / s.total_bytes) * 100 : 0;

  row.className = `t s-${st}` + (state.selected === t.id ? ' selected' : '');
  $('.t-name', row).textContent = t.name || t.info_hash;
  $('.bar i', row).style.width = `${pct.toFixed(2)}%`;

  const paused = isPaused(t);
  const btn = $('.a-toggle', row);
  const want = paused ? 'play' : 'pause';
  if (btn.dataset.i !== want) {
    btn.dataset.i = want;
    btn.innerHTML = icon(want);
    btn.title = paused ? 'Продолжить' : 'Пауза';
  }

  const parts = [`<span class="badge">${STATUS[st]}</span>`];
  parts.push(`<span>${pct >= 100 ? fmtBytes(s.total_bytes) : `${fmtBytes(s.progress_bytes)} из ${fmtBytes(s.total_bytes)}`}</span>`);
  if (pct < 100) parts.push(`<span>${pct.toFixed(1)}%</span>`);
  if (live) {
    if (!s.finished) parts.push(`<span class="dl">${icon('down')}${fmtSpeed(live.download_speed?.mbps)}</span>`);
    parts.push(`<span class="ul">${icon('up')}${fmtSpeed(live.upload_speed?.mbps)}</span>`);
    const ps = live.snapshot?.peer_stats;
    if (ps) parts.push(`<span>${icon('peers')}${ps.live ?? 0}${ps.seen != null ? ` / ${ps.seen}` : ''}</span>`);
    const eta = live.time_remaining?.duration?.secs;
    if (!s.finished && eta != null) parts.push(`<span>осталось ${fmtEta(eta)}</span>`);
  }
  const meta = $('.t-meta', row);
  meta.innerHTML = parts.join('');
  if (st === 'error' && s.error) meta.append(el('span', null, s.error));
}

function renderList() {
  const q = state.query.trim().toLowerCase();
  const visible = state.torrents
    .filter((t) => matches(t, state.filter))
    .filter((t) => !q || (t.name || '').toLowerCase().includes(q));

  const list = $('#list');
  const seen = new Set();
  let prev = null;
  for (const t of visible) {
    seen.add(t.id);
    let row = rows.get(t.id);
    if (!row) { row = makeRow(t); rows.set(t.id, row); }
    updateRow(row, t);
    const expected = prev ? prev.nextSibling : list.firstChild;
    if (row !== expected) list.insertBefore(row, expected);
    prev = row;
  }
  for (const [id, row] of rows) {
    if (!seen.has(id)) { row.remove(); rows.delete(id); }
  }
  $('#empty').classList.toggle('hidden', visible.length > 0);
  $('#empty h2').textContent = state.torrents.length ? 'Ничего не найдено' : 'Пока пусто';
  $('#empty p').classList.toggle('hidden', state.torrents.length > 0);

  for (const b of $$('#filters button')) {
    const n = state.torrents.filter((t) => matches(t, b.dataset.f)).length;
    $('b', b).textContent = n || '';
  }
}

// ---------- График скорости ----------
function drawGraph() {
  const c = $('#graph');
  const dpr = window.devicePixelRatio || 1;
  const w = c.clientWidth, h = c.clientHeight;
  if (c.width !== w * dpr) { c.width = w * dpr; c.height = h * dpr; }
  const ctx = c.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  const css = getComputedStyle(document.documentElement);
  const N = 60;
  const data = state.history;
  const max = Math.max(0.01, ...data.map((d) => Math.max(d[0], d[1]))) * 1.15;
  const line = (idx, color, fill) => {
    if (!data.length) return;
    ctx.beginPath();
    data.forEach((d, i) => {
      const x = w - (data.length - 1 - i) * (w / (N - 1));
      const y = h - 1 - (d[idx] / max) * (h - 4);
      i ? ctx.lineTo(x, y) : ctx.moveTo(x, y);
    });
    ctx.strokeStyle = color; ctx.lineWidth = 1.6; ctx.lineJoin = 'round'; ctx.stroke();
    if (fill) {
      ctx.lineTo(w, h); ctx.lineTo(w - (data.length - 1) * (w / (N - 1)), h); ctx.closePath();
      const g = ctx.createLinearGradient(0, 0, 0, h);
      g.addColorStop(0, color + '55'); g.addColorStop(1, color + '00');
      ctx.fillStyle = g; ctx.fill();
    }
  };
  line(1, css.getPropertyValue('--green').trim(), false);
  line(0, css.getPropertyValue('--accent').trim(), true);
}

// ---------- Опрос ----------
let engineOk = false;
async function tick() {
  try {
    const snap = await invoke('snapshot');
    state.torrents = snap.torrents.sort((a, b) => b.id - a.id);
    state.settings = snap.settings;
    const down = snap.session?.download_speed?.mbps || 0;
    const up = snap.session?.upload_speed?.mbps || 0;
    state.history.push([down, up]);
    if (state.history.length > 60) state.history.shift();
    $('#g-down').textContent = fmtSpeed(down);
    $('#g-up').textContent = fmtSpeed(up);
    const st = $('#st-engine');
    st.className = 'dot ok';
    st.textContent = `Раздач: ${state.torrents.length}`;
    const livePeers = snap.session?.peers?.live;
    $('#st-peers').textContent = livePeers != null ? `Пиров: ${livePeers}` : '';
    $('#st-port').textContent = snap.port ? `Порт: ${snap.port}` : '';
    engineOk = true;
    renderList();
    drawGraph();
    if (state.selected != null) {
      if (!state.torrents.some((t) => t.id === state.selected)) select(null);
      else renderDetail();
    }
  } catch (err) {
    engineOk = false;
    const st = $('#st-engine');
    st.className = 'dot bad';
    st.textContent = String(err);
  }
}

// ---------- Действия ----------
async function toggle(id) {
  const t = state.torrents.find((x) => x.id === id);
  if (!t) return;
  try {
    await invoke(isPaused(t) ? 'resume' : 'pause', { id });
    tick();
  } catch (err) { fail(err); }
}

let removeId = null;
function askRemove(id) {
  const t = state.torrents.find((x) => x.id === id);
  if (!t) return;
  removeId = id;
  $('#rm-name').textContent = t.name || t.info_hash;
  $('#rm-files').checked = false;
  openModal('m-remove');
}
$('#rm-ok').addEventListener('click', async () => {
  const id = removeId;
  closeModals();
  try {
    await invoke('remove', { id, deleteFiles: $('#rm-files').checked });
    if (state.selected === id) select(null);
    tick();
  } catch (err) { fail(err); }
});

// ---------- Панель подробностей ----------
async function select(id) {
  state.selected = id;
  state.details = null;
  $('#detail').classList.toggle('hidden', id == null);
  for (const [rid, row] of rows) row.classList.toggle('selected', rid === id);
  if (id == null) return;
  $('#detail-body').textContent = '';
  try {
    const d = await invoke('details', { id });
    if (state.selected === id) { state.details = { id, files: d.files || [] }; renderDetail(true); }
  } catch (err) { fail(err); }
}

let peersBusy = false;
async function renderDetail(force) {
  const t = state.torrents.find((x) => x.id === state.selected);
  const body = $('#detail-body');
  if (!t) return;
  const s = t.stats || {};

  if (state.tab === 'files') {
    const files = state.details?.files;
    if (!files) return;
    if (!files.length) {
      // Метаданные magnet-ссылки ещё не получены — пробуем снова.
      body.innerHTML = '<p class="muted">Получение метаданных…</p>';
      invoke('details', { id: t.id }).then((d) => {
        if (state.selected === t.id && d.files?.length) { state.details = { id: t.id, files: d.files }; renderDetail(true); }
      }).catch(() => {});
      return;
    }
    let table = $('table.files', body);
    if (!table || force) {
      body.textContent = '';
      table = el('table', 'files');
      table.innerHTML = '<thead><tr><th></th><th>Файл</th><th class="num">Размер</th><th>Прогресс</th><th class="num"></th></tr></thead>';
      const tb = el('tbody');
      files.forEach((f, i) => {
        if (f.attributes?.padding) return;
        const tr = el('tr');
        tr.dataset.i = i;
        const cb = el('input'); cb.type = 'checkbox'; cb.checked = f.included;
        cb.addEventListener('change', () => { f.included = cb.checked; saveFiles(t.id); });
        const td0 = el('td'); td0.append(cb);
        tr.append(td0, el('td', 'name', f.name), el('td', 'num', fmtBytes(f.length)));
        const tdBar = el('td'); tdBar.innerHTML = '<div class="bar"><i></i></div>';
        tr.append(tdBar, el('td', 'num pct'));
        tb.append(tr);
      });
      table.append(tb);
      body.append(table);
    }
    for (const tr of $$('tbody tr', table)) {
      const i = +tr.dataset.i;
      const len = files[i].length;
      const pct = len ? Math.min(100, ((s.file_progress?.[i] || 0) / len) * 100) : 100;
      $('.bar i', tr).style.width = `${pct}%`;
      $('.pct', tr).textContent = `${pct.toFixed(pct >= 100 ? 0 : 1)}%`;
    }
  } else if (state.tab === 'peers') {
    if (peersBusy) return;
    peersBusy = true;
    try {
      const res = await invoke('peers', { id: t.id });
      if (state.tab !== 'peers' || state.selected !== t.id) return;
      const list = Object.entries(res.peers || {})
        .sort((a, b) => b[1].counters.fetched_bytes - a[1].counters.fetched_bytes);
      body.textContent = '';
      if (!list.length) { body.append(el('p', 'muted', 'Нет подключённых пиров')); return; }
      const table = el('table');
      table.innerHTML = '<thead><tr><th>Адрес</th><th>Клиент</th><th>Тип</th><th class="num">Получено</th><th class="num">Отдано</th></tr></thead>';
      const tb = el('tbody');
      for (const [addr, p] of list) {
        const tr = el('tr');
        tr.append(
          el('td', 'name', addr),
          el('td', null, p.client_name || '—'),
          el('td', null, p.conn_kind ? String(p.conn_kind).toUpperCase() : '—'),
          el('td', 'num', fmtBytes(p.counters.fetched_bytes)),
          el('td', 'num', fmtBytes(p.counters.uploaded_bytes)),
        );
        tb.append(tr);
      }
      table.append(tb);
      body.append(table);
    } catch (err) {
      body.textContent = String(err);
    } finally { peersBusy = false; }
  } else {
    const ratio = s.progress_bytes ? (s.uploaded_bytes / s.progress_bytes).toFixed(2) : '0.00';
    const rowsInfo = [
      ['Название', t.name || '—'],
      ['Состояние', STATUS[statusOf(t)] + (s.error ? ` — ${s.error}` : '')],
      ['Размер', fmtBytes(s.total_bytes)],
      ['Скачано', fmtBytes(s.progress_bytes)],
      ['Отдано', `${fmtBytes(s.uploaded_bytes)} (рейтинг ${ratio})`],
      ['Частей', String(t.total_pieces || '—')],
      ['Папка', t.output_folder],
      ['Info hash', t.info_hash],
    ];
    const dl = el('dl', 'info');
    for (const [k, v] of rowsInfo) dl.append(el('dt', null, k), el('dd', null, v));
    body.textContent = '';
    body.append(dl);
  }
}

let saveTimer = null;
function saveFiles(id) {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(async () => {
    const files = state.details?.files;
    if (!files || state.details.id !== id) return;
    const sel = files.map((f, i) => (f.included ? i : -1)).filter((i) => i >= 0);
    if (!sel.length) { fail('Нужно оставить хотя бы один файл'); return; }
    try { await invoke('set_files', { id, files: sel }); } catch (err) { fail(err); }
  }, 400);
}

for (const b of $$('.tabs button')) {
  b.addEventListener('click', () => {
    state.tab = b.dataset.t;
    $$('.tabs button').forEach((x) => x.classList.toggle('active', x === b));
    $('#detail-body').textContent = '';
    renderDetail(true);
  });
}
$('#detail-close').addEventListener('click', () => select(null));

// ---------- Фильтры и поиск ----------
for (const b of $$('#filters button')) {
  b.addEventListener('click', () => {
    state.filter = b.dataset.f;
    $$('#filters button').forEach((x) => x.classList.toggle('active', x === b));
    $('#title').textContent = b.dataset.f === 'all' ? 'Все раздачи' : $('span', b).textContent;
    renderList();
  });
}
$('#search').addEventListener('input', (ev) => { state.query = ev.target.value; renderList(); });

// ---------- Модальные окна ----------
function openModal(id) { closeModals(); $('#' + id).classList.remove('hidden'); }
function closeModals() { $$('.modal').forEach((m) => m.classList.add('hidden')); }
for (const m of $$('.modal')) {
  m.addEventListener('mousedown', (ev) => { if (ev.target === m) closeModals(); });
  $$('[data-close]', m).forEach((b) => b.addEventListener('click', closeModals));
}
document.addEventListener('keydown', (ev) => {
  if (ev.key === 'Escape') closeModals();
  if (ev.key === 'Enter' && !$('#m-add').classList.contains('hidden')) $('#add-next').click();
});

// ---------- Добавление ----------
let pending = null; // результат preview
function showError(id, err) {
  const box = $('#' + id);
  box.textContent = err ? String(err) : '';
  box.classList.toggle('hidden', !err);
}
function setBusy(btn, busy, label) {
  btn.disabled = busy;
  btn.innerHTML = busy ? `<span class="spin"></span>${label}` : label;
}

function openAdd(source) {
  pending = null;
  $('#add-source').value = source || '';
  $('#add-step1').classList.remove('hidden');
  $('#add-step2').classList.add('hidden');
  $('#add-next').textContent = 'Далее';
  $('#add-next').disabled = false;
  showError('add-error');
  openModal('m-add');
  if (source) loadPreview(); else $('#add-source').focus();
}

async function loadPreview() {
  const source = $('#add-source').value.trim();
  if (!source) return;
  const btn = $('#add-next');
  showError('add-error');
  setBusy(btn, true, source.startsWith('magnet:') ? 'Поиск пиров…' : 'Чтение…');
  try {
    const res = await invoke('preview', { source });
    if ($('#m-add').classList.contains('hidden')) return;
    pending = { source, res };
    const files = res.details.files || [];
    const total = files.reduce((a, f) => a + f.length, 0);
    $('#add-name').textContent = res.details.name || res.details.info_hash;
    const box = $('#add-files');
    box.textContent = '';
    files.forEach((f, i) => {
      if (f.attributes?.padding) return;
      const lab = el('label');
      const cb = el('input'); cb.type = 'checkbox'; cb.checked = true; cb.dataset.i = i;
      cb.addEventListener('change', updateAddMeta);
      lab.append(cb, el('span', null, f.name), el('em', null, fmtBytes(f.length)));
      box.append(lab);
    });
    pending.total = total;
    $('#add-all').checked = true;
    $('#add-folder').value = state.settings?.download_dir || '';
    $('#add-paused').checked = false;
    $('#add-remember').checked = false;
    updateAddMeta();
    $('#add-step1').classList.add('hidden');
    $('#add-step2').classList.remove('hidden');
    setBusy(btn, false, 'Скачать');
  } catch (err) {
    showError('add-error', err);
    setBusy(btn, false, 'Далее');
  }
}

function updateAddMeta() {
  if (!pending) return;
  const files = pending.res.details.files || [];
  const cbs = $$('#add-files input');
  const sel = cbs.filter((c) => c.checked);
  const size = sel.reduce((a, c) => a + files[+c.dataset.i].length, 0);
  $('#add-meta').textContent = `Выбрано ${sel.length} из ${cbs.length} · ${fmtBytes(size)} из ${fmtBytes(pending.total)}`;
  $('#add-all').checked = sel.length === cbs.length;
}

async function confirmAdd() {
  const btn = $('#add-next');
  const cbs = $$('#add-files input');
  const sel = cbs.filter((c) => c.checked).map((c) => +c.dataset.i);
  if (!sel.length) { showError('add-error', 'Выберите хотя бы один файл'); return; }
  showError('add-error');
  setBusy(btn, true, 'Добавление…');
  try {
    await invoke('add', {
      source: pending.source,
      onlyFiles: sel.length === cbs.length ? null : sel,
      folder: $('#add-folder').value.trim() || null,
      name: pending.res.details.name || null,
      multi: (pending.res.details.files || []).length > 1,
      paused: $('#add-paused').checked,
      peers: pending.res.seen_peers || null,
    });
    const folder = $('#add-folder').value.trim();
    if ($('#add-remember').checked && folder) await invoke('set_download_dir', { dir: folder }).catch(fail);
    closeModals();
    toast('Раздача добавлена');
    tick();
  } catch (err) {
    showError('add-error', err);
    setBusy(btn, false, 'Скачать');
  }
}

$('#btn-add').addEventListener('click', () => openAdd(''));
$('#add-next').addEventListener('click', () => (pending ? confirmAdd() : loadPreview()));
$('#add-browse').addEventListener('click', async () => {
  const p = await invoke('pick_torrent_file').catch(fail);
  if (p) { $('#add-source').value = p; loadPreview(); }
});
$('#add-pickfolder').addEventListener('click', async () => {
  const p = await invoke('pick_folder').catch(fail);
  if (p) $('#add-folder').value = p;
});
$('#add-all').addEventListener('change', (ev) => {
  $$('#add-files input').forEach((c) => { c.checked = ev.target.checked; });
  updateAddMeta();
});

// Вставка magnet-ссылки из буфера обмена в любом месте окна.
document.addEventListener('paste', (ev) => {
  if (ev.target instanceof HTMLInputElement) return;
  const text = (ev.clipboardData?.getData('text') || '').trim();
  if (/^(magnet:|https?:\/\/)/i.test(text)) openAdd(text);
});

// Перетаскивание .torrent-файлов.
listen('tauri://drag-enter', () => $('#drop').classList.remove('hidden'));
listen('tauri://drag-leave', () => $('#drop').classList.add('hidden'));
listen('tauri://drag-drop', (ev) => {
  $('#drop').classList.add('hidden');
  const file = (ev.payload?.paths || []).find((p) => p.toLowerCase().endsWith('.torrent'));
  if (file) openAdd(file); else toast('Это не .torrent-файл', true);
});

// ---------- Настройки ----------
$('#btn-settings').addEventListener('click', () => {
  const s = state.settings || {};
  $('#set-dir').value = s.download_dir || '';
  $('#set-down').value = s.down_limit_kbps || '';
  $('#set-up').value = s.up_limit_kbps || '';
  $('#set-tray').checked = s.close_to_tray !== false;
  showError('set-error');
  refreshAssoc();
  openModal('m-settings');
});
async function refreshAssoc() {
  const ok = await invoke('is_associated').catch(() => false);
  $('#set-assoc').disabled = ok;
  $('#set-assoc-state').textContent = ok
    ? 'Phoenix уже зарегистрирован для .torrent-файлов и magnet-ссылок.'
    : 'Если для .torrent уже выбрана другая программа, Windows может спросить, чем открывать.';
}
$('#set-assoc').addEventListener('click', async () => {
  try { await invoke('associate'); toast('Готово: .torrent и magnet открываются в Phoenix'); }
  catch (err) { showError('set-error', err); }
  refreshAssoc();
});
// Ссылка или файл, переданные при повторном запуске программы.
listen('open-source', (ev) => { if (ev.payload) openAdd(ev.payload); });
$('#set-pick').addEventListener('click', async () => {
  const p = await invoke('pick_folder').catch(fail);
  if (p) $('#set-dir').value = p;
});
$('#set-save').addEventListener('click', async () => {
  try {
    const dir = $('#set-dir').value.trim();
    if (dir) await invoke('set_download_dir', { dir });
    await invoke('set_close_to_tray', { value: $('#set-tray').checked });
    await invoke('set_limits', {
      down: Math.max(0, parseInt($('#set-down').value, 10) || 0),
      up: Math.max(0, parseInt($('#set-up').value, 10) || 0),
    });
    closeModals();
    toast('Настройки сохранены');
    tick();
  } catch (err) { showError('set-error', err); }
});

// ---------- Запуск ----------
document.addEventListener('contextmenu', (ev) => {
  if (!(ev.target instanceof HTMLInputElement)) ev.preventDefault();
});

(async () => {
  await tick();
  setInterval(tick, 1000);
  const arg = await invoke('startup_arg').catch(() => null);
  if (arg) openAdd(arg);
})();
