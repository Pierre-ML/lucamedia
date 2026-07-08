// Logique client : état de la file, SSE, rendu des listes.
// Toute la logique métier vit côté serveur ; ici on ne fait qu'afficher et appeler l'API.

const $ = (id) => document.getElementById(id);

let queue = null; // dernier instantané reçu du serveur

// ---------- Icônes (SVG inline, trait 1.8, aucune dépendance) ----------

const svg = (paths, size = 'size-3.5') =>
  `<svg class="${size} shrink-0" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths}</svg>`;

const ICONS = {
  play: svg('<path d="M7 4.5v15l13-7.5z" fill="currentColor" stroke="none"/>', 'size-4'),
  pause: svg(
    '<rect x="6" y="4.5" width="4" height="15" rx="1" fill="currentColor" stroke="none"/><rect x="14" y="4.5" width="4" height="15" rx="1" fill="currentColor" stroke="none"/>',
    'size-4'
  ),
  x: svg('<path d="M6 6l12 12M18 6L6 18"/>'),
  retry: svg('<path d="M20 11a8 8 0 1 0-2.34 6.34"/><path d="M20 5v6h-6"/>'),
  check: svg('<path d="M4.5 12.5l5 5 10-11"/>', 'size-4'),
};

// ---------- Helpers ----------

const escapeDiv = document.createElement('div');

function escapeHtml(text) {
  escapeDiv.textContent = text ?? '';
  return escapeDiv.innerHTML;
}

// Mémo de rendu : évite de reconstruire une liste dont les données n'ont pas changé
// (préserve les animations en cours et l'état de survol, et économise du DOM)
const renderCache = new Map();

function shouldRender(key, data) {
  const json = JSON.stringify(data);
  if (renderCache.get(key) === json) return false;
  renderCache.set(key, json);
  return true;
}

function formatSize(mb) {
  if (mb == null) return '—';
  return mb >= 1024 ? `${(mb / 1024).toFixed(2)} Go` : `${mb.toFixed(1)} Mo`;
}

function formatEta(seconds) {
  if (seconds == null) return '—';
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  const pad = (n) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

function formatDate(sqliteDate) {
  if (!sqliteDate) return '';
  return new Date(sqliteDate.replace(' ', 'T')).toLocaleString('fr-FR', {
    dateStyle: 'short',
    timeStyle: 'short',
  });
}

async function api(path, options = {}) {
  const res = await fetch(`/api${path}`, {
    headers: { 'Content-Type': 'application/json' },
    ...options,
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new Error(body.error ?? `Erreur HTTP ${res.status}`);
  }
  return res.json();
}

let toastTimer = null;

function toast(message) {
  let el = document.querySelector('.toast');
  if (!el) {
    el = document.createElement('div');
    el.className = 'toast';
    el.setAttribute('role', 'status');
    document.body.appendChild(el);
  }
  el.textContent = message;
  el.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove('show'), 3500);
}

function setOnline(online) {
  const pill = $('connection-status');
  pill.dataset.online = online ? '1' : '0';
  pill.textContent = online ? 'serveur en ligne' : 'serveur hors ligne';
}

// ---------- Console yt-dlp ----------

const LOG_LIMIT = 300;
const consoleBody = $('console-body');
let consoleCount = 0;

function logLineEl(stream, line) {
  const div = document.createElement('div');
  div.textContent = line;
  div.className =
    stream === 'err'
      ? 'text-danger bg-danger/[0.04] px-1.5 py-0.5 rounded border border-danger/10 font-semibold'
      : stream === 'app'
      ? 'text-accent font-semibold'
      : 'text-ink-300';
  return div;
}

function appendLog({ stream, line, replace }) {
  consoleBody.querySelector('.console-placeholder')?.remove();
  // On reste collé en bas sauf si l'utilisateur a remonté la console
  const pinned =
    consoleBody.scrollHeight - consoleBody.scrollTop - consoleBody.clientHeight < 48;
  if (replace && consoleBody.lastElementChild) {
    // Chemin rapide : les lignes de progression successives partagent le même style,
    // on ne remplace que le texte au lieu de recréer un élément
    const last = consoleBody.lastElementChild;
    const fresh = logLineEl(stream, line);
    if (last.className === fresh.className) last.textContent = line;
    else last.replaceWith(fresh);
  } else {
    consoleBody.appendChild(logLineEl(stream, line));
    consoleCount += 1;
    if (consoleCount > LOG_LIMIT) {
      consoleBody.firstElementChild?.remove();
      consoleCount -= 1;
    }
  }
  if (pinned) consoleBody.scrollTop = consoleBody.scrollHeight;
}

function clearConsole(message) {
  consoleBody.innerHTML = `<p class="console-placeholder text-ink-600 italic">// ${message}</p>`;
  consoleCount = 0;
}

function setConsoleVisible(visible) {
  consoleBody.classList.toggle('hidden', !visible);
  $('console-toggle').textContent = visible ? 'Masquer' : 'Afficher';
  localStorage.setItem('console-hidden', visible ? '0' : '1');
}

$('console-clear').addEventListener('click', () => clearConsole('Console vidée'));
$('console-toggle').addEventListener('click', () =>
  setConsoleVisible(consoleBody.classList.contains('hidden'))
);
if (localStorage.getItem('console-hidden') === '1') setConsoleVisible(false);

// ---------- Rendu ----------

function renderToggle(state) {
  const btn = $('toggle-state');
  btn.dataset.state = state;
  btn.innerHTML =
    state === 'pause' ? `${ICONS.play}<span>Reprendre</span>` : `${ICONS.pause}<span>Pause</span>`;
}

function renderCurrent(current) {
  const empty = $('current-empty');
  const card = $('current-card');
  if (!current) {
    empty.classList.remove('hidden');
    card.classList.add('hidden');
    $('current-retries').classList.add('hidden');
    return;
  }
  empty.classList.add('hidden');
  card.classList.remove('hidden');
  $('current-name').textContent = current.filename;
  $('current-url').textContent = current.url;
  updateProgress(current);
}

function updateProgress(data) {
  const percent = Math.min(100, data.progress_percent ?? 0);
  $('current-percent').textContent = `${percent.toFixed(1)}%`;
  $('current-bar').style.width = `${percent}%`;
  $('current-size').textContent = formatSize(data.size_estimate_mb);
  $('current-eta').textContent = formatEta(data.eta_seconds);
  const retries = $('current-retries');
  if (data.retry_count > 0) {
    retries.textContent = `tentative ${data.retry_count + 1}`;
    retries.classList.remove('hidden');
  } else {
    retries.classList.add('hidden');
  }
}

function renderPending(pending) {
  if (!shouldRender('pending', pending)) return;
  $('pending-count').textContent = pending.length;
  $('pending-empty').classList.toggle('hidden', pending.length > 0);
  $('pending-list').innerHTML = pending
    .map(
      (d, i) => `
      <li class="hover-row-effect flex items-center gap-4 px-4 py-3.5 transition-all duration-150 border-b-2 border-black last:border-0 bg-ink-900/50">
        <span class="w-6 shrink-0 text-center font-mono text-[10px] font-extrabold text-black bg-cyber-yellow border-2 border-black shadow-[1.5px_1.5px_0px_#000] py-0.5 rounded">${i + 1}</span>
        <div class="min-w-0 flex-1">
          <p class="truncate text-sm font-bold text-ink-200 tracking-tight">${escapeHtml(d.filename)}</p>
          <p class="truncate font-mono text-[10px] text-ink-500 mt-0.5" title="${escapeHtml(d.url)}">${escapeHtml(d.url)}</p>
        </div>
        <div class="row-actions flex shrink-0 gap-1.5">
          <button type="button" class="border-2 border-black bg-danger text-black px-2.5 py-1 rounded text-[10px] font-black uppercase tracking-wider shadow-[2px_2px_0px_#000] hover:translate-x-[0.5px] hover:translate-y-[0.5px] hover:shadow-[1.5px_1.5px_0px_#000] active:translate-x-[2px] active:translate-y-[2px] active:shadow-[0px_0px_0px_#000] transition-all duration-75 cursor-pointer" data-action="delete" data-id="${d.id}">
            Retirer
          </button>
        </div>
      </li>`
    )
    .join('');
}

function renderErrors(errors) {
  if (!shouldRender('errors', errors)) return;
  $('error-count').textContent = errors.length;
  $('error-empty').classList.toggle('hidden', errors.length > 0);
  $('error-list').innerHTML = errors
    .map(
      (d) => `
      <li class="hover-row-effect px-4 py-4 transition-all duration-150 border-b-2 border-black last:border-0 bg-danger/[0.03]">
        <div class="flex items-center gap-4">
          <span class="relative flex size-2 shrink-0">
            <span class="animate-ping absolute inline-flex h-full w-full rounded-full bg-danger opacity-75"></span>
            <span class="relative inline-flex rounded-full size-2 bg-danger border border-black"></span>
          </span>
          <div class="min-w-0 flex-1">
            <p class="truncate text-sm font-bold text-ink-200 tracking-tight">${escapeHtml(d.filename)}</p>
            <p class="truncate font-mono text-[10px] text-ink-500 mt-0.5" title="${escapeHtml(d.url)}">${escapeHtml(d.url)}</p>
          </div>
          <div class="row-actions flex shrink-0 gap-1.5">
            <button type="button" class="border-2 border-black bg-cyber-cyan text-black px-2.5 py-1 rounded text-[10px] font-black uppercase tracking-wider shadow-[2px_2px_0px_#000] hover:translate-x-[0.5px] hover:translate-y-[0.5px] hover:shadow-[1.5px_1.5px_0px_#000] active:translate-x-[2px] active:translate-y-[2px] active:shadow-[0px_0px_0px_#000] transition-all duration-75 cursor-pointer" data-action="retry" data-id="${d.id}">
              Relancer
            </button>
            <button type="button" class="border-2 border-black bg-danger text-black px-2.5 py-1 rounded text-[10px] font-black uppercase tracking-wider shadow-[2px_2px_0px_#000] hover:translate-x-[0.5px] hover:translate-y-[0.5px] hover:shadow-[1.5px_1.5px_0px_#000] active:translate-x-[2px] active:translate-y-[2px] active:shadow-[0px_0px_0px_#000] transition-all duration-75 cursor-pointer" data-action="delete" data-id="${d.id}">
              Retirer
            </button>
          </div>
        </div>
        <div class="mt-2.5 ml-6 rounded border-2 border-black bg-danger/10 p-2.5 font-mono text-[10px] text-danger font-bold leading-normal break-all shadow-[2px_2px_0px_#000]">
          ${escapeHtml((d.error_message ?? '').split('\n').at(-1))}
        </div>
      </li>`
    )
    .join('');
}

function renderDone(done) {
  if (!shouldRender('done', done)) return;
  $('done-count').textContent = done.length;
  $('done-empty').classList.toggle('hidden', done.length > 0);
  $('done-list').innerHTML = done
    .map(
      (d) => `
      <li class="hover-row-effect flex items-center gap-4 px-4 py-3.5 transition-all duration-150 border-b-2 border-black last:border-0 bg-success/[0.02]">
        <span class="shrink-0 text-success">${ICONS.check}</span>
        <div class="min-w-0 flex-1">
          <p class="truncate text-sm font-bold text-ink-200 tracking-tight">${escapeHtml(d.filename)}</p>
          <p class="truncate font-mono text-[10px] text-ink-500 mt-0.5" title="${escapeHtml(d.url)}">${escapeHtml(d.url)}</p>
        </div>
        <span class="shrink-0 font-mono text-[10px] font-bold text-black bg-accent border-2 border-black px-2 py-0.5 rounded shadow-[1.5px_1.5px_0px_#000] tabular-nums">${formatDate(d.updated_at)}</span>
        <div class="row-actions flex shrink-0">
          <button type="button" class="border-2 border-black bg-danger text-black p-1 rounded shadow-[2px_2px_0px_#000] hover:translate-x-[0.5px] hover:translate-y-[0.5px] hover:shadow-[1.5px_1.5px_0px_#000] active:translate-x-[2px] active:translate-y-[2px] active:shadow-[0px_0px_0px_#000] transition-all duration-75 cursor-pointer" data-action="delete" data-id="${d.id}"
            title="Retirer de l'historique" aria-label="Retirer de l'historique">
            ${ICONS.x}
          </button>
        </div>
      </li>`
    )
    .join('');
}

function renderQueue(snapshot) {
  queue = snapshot;
  $('output-dir').textContent = snapshot.outputDir;
  $('output-dir').title = snapshot.outputDir;
  renderToggle(snapshot.state);
  renderCurrent(snapshot.current);
  renderPending(snapshot.pending);
  renderErrors(snapshot.errors);
  renderDone(snapshot.done);
}

// ---------- Actions ----------

$('add-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const feedback = $('add-feedback');
  const urls = $('add-urls')
    .value.split('\n')
    .map((line) => line.trim())
    .filter(Boolean);
  if (urls.length === 0) return;

  const filename = $('add-filename').value.trim();
  const uqload = $('add-uqload').checked;
  const items = urls.map((url, i) => ({
    url,
    // le nom de fichier ne s'applique qu'à un ajout simple
    filename: urls.length === 1 && i === 0 ? filename : '',
    uqload,
  }));

  try {
    const result = await api('/queue', { method: 'POST', body: JSON.stringify({ items }) });
    $('add-urls').value = '';
    $('add-filename').value = '';
    feedback.textContent =
      result.rejected.length > 0
        ? `${result.added.length} lien(s) ajouté(s), ${result.rejected.length} invalide(s) ignoré(s)`
        : `${result.added.length} lien(s) ajouté(s) à la file`;
    // on ne change que la couleur pour préserver les classes de mise en forme du composant
    feedback.classList.remove('text-danger');
    feedback.classList.add('text-success');
  } catch (err) {
    feedback.textContent = err.message;
    feedback.classList.remove('text-success');
    feedback.classList.add('text-danger');
  }
  setTimeout(() => (feedback.textContent = ''), 4000);
});

$('toggle-state').addEventListener('click', async () => {
  if (!queue) return;
  const action = queue.state === 'pause' ? 'play' : 'pause';
  await api(`/control/${action}`, { method: 'POST' }).catch((err) => toast(err.message));
});

$('open-folder').addEventListener('click', () => {
  api('/open-folder', { method: 'POST' }).catch((err) => toast(err.message));
});

// Délégation pour les boutons Retirer / Relancer des listes
document.addEventListener('click', async (e) => {
  const btn = e.target.closest('button[data-action]');
  if (!btn) return;
  const { action, id } = btn.dataset;
  try {
    if (action === 'delete') await api(`/queue/${id}`, { method: 'DELETE' });
    if (action === 'retry') await api(`/queue/${id}/retry`, { method: 'POST' });
  } catch (err) {
    toast(err.message);
  }
});

// ---------- Temps réel (SSE) ----------

function connect() {
  const source = new EventSource('/api/events');

  source.addEventListener('queue', (e) => {
    setOnline(true);
    renderQueue(JSON.parse(e.data));
  });

  source.addEventListener('progress', (e) => {
    const data = JSON.parse(e.data);
    if (queue?.current?.id === data.id) updateProgress(data);
  });

  source.addEventListener('log', (e) => {
    appendLog(JSON.parse(e.data));
  });

  source.onopen = () => {
    setOnline(true);
    // resynchronise l'état complet après une (re)connexion
    api('/queue').then(renderQueue).catch(() => {});
  };

  source.onerror = () => {
    setOnline(false);
    // EventSource retente automatiquement la connexion
  };
}

renderToggle('play');
api('/queue')
  .then((snapshot) => {
    setOnline(true);
    renderQueue(snapshot);
  })
  .catch(() => setOnline(false));
api('/logs')
  .then((lines) => lines.forEach(appendLog))
  .catch(() => {});
connect();
