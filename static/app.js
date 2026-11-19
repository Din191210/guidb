(function () {
  "use strict";

  const state = {
    connections: [],
    workspace: null,
    history: [],
    savedQueries: [],
    stagedMutations: [],
    insertedRows: [],
    updatedCells: new Map(), // key: `${rowIdx}:${colName}` -> CellValue
    deletedRowIndices: new Set(),
    offset: 0,
    limit: 100,
    requestId: 0,
    queryResult: false,
    copiedTable: null,
    pasting: false,
    saving: false,
    sortCol: null,
    sortDir: "asc",
    filterText: "",
    activeTab: "data",
    lastSyncDiff: null,
    wasm: null,
  };

  // --- Pure-Rust WebAssembly Bridge ---
  async function initWasm() {
    const badge = document.getElementById("footer-wasm-badge");
    try {
      const resp = await fetch("/static/guidb_wasm.wasm");
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const bytes = await resp.arrayBuffer();
      const { instance } = await WebAssembly.instantiate(bytes, {});
      state.wasm = instance.exports;
      if (badge) badge.textContent = "WASM Engine: Aktif (guidb_wasm.wasm)";
    } catch (err) {
      if (badge) badge.textContent = `WASM Engine: Fallback (${err.message})`;
    }
  }

  function callWasmStringFn(fnName, inputStr) {
    if (!state.wasm || typeof state.wasm[fnName] !== "function") return null;
    const enc = new TextEncoder();
    const dec = new TextDecoder();
    const bytes = enc.encode(inputStr);
    const inPtr = state.wasm.wasm_alloc(bytes.length);
    if (!inPtr && bytes.length > 0) return null;
    new Uint8Array(state.wasm.memory.buffer, inPtr, bytes.length).set(bytes);
    const packed = BigInt.asUintN(64, state.wasm[fnName](inPtr, bytes.length));
    state.wasm.wasm_free(inPtr, bytes.length);

    const outPtr = Number(packed >> 32n);
    const outLen = Number(packed & 0xffffffffn);
    if (!outPtr && outLen === 0) return "";
    const outBytes = new Uint8Array(state.wasm.memory.buffer, outPtr, outLen);
    const result = dec.decode(outBytes.slice());
    state.wasm.wasm_free(outPtr, outLen);
    return result;
  }

  function wasmFormatSql(text) {
    const out = callWasmStringFn("wasm_format_sql", text);
    return out !== null ? out : text;
  }

  function wasmPreviewMutations(engine, mutations) {
    const payload = JSON.stringify({ engine, mutations });
    const out = callWasmStringFn("wasm_preview_mutations", payload);
    if (!out) return [];
    try {
      return JSON.parse(out);
    } catch {
      return [out];
    }
  }

  // --- Helpers ---
  async function apiJson(url, opts = {}) {
    const res = await fetch(url, {
      headers: { "Content-Type": "application/json", ...(opts.headers || {}) },
      ...opts,
    });
    if (res.status === 204) return null;
    const text = await res.text();
    let data = {};
    try {
      data = text ? JSON.parse(text) : {};
    } catch {
      data = { error: text };
    }
    if (!res.ok) {
      throw new Error(data.error || `Request failed (${res.status})`);
    }
    return data;
  }

  function showBanner(msg, kind = "info") {
    const el = document.getElementById("status-banner");
    if (!el) return;
    el.hidden = false;
    el.className = `status-banner ${kind === "error" ? "is-error" : kind === "success" ? "is-success" : ""}`;
    el.textContent = msg;
  }

  function clearBanner() {
    const el = document.getElementById("status-banner");
    if (el) el.hidden = true;
  }

  function cellToDisplay(cell) {
    if (!cell || cell.kind === "null") return "NULL";
    if (cell.kind === "json") return JSON.stringify(cell.value);
    return String(cell.value);
  }

  function parseInputToCell(raw, colType, parseNull = true) {
    const trimmed = raw.trim();
    if (parseNull && trimmed.toUpperCase() === "NULL") return { kind: "null" };
    const t = (colType || "").toUpperCase();
    if (t.includes("INT") && /^-?\d+$/.test(trimmed)) {
      return { kind: "int", value: Number(trimmed) };
    }
    if ((t.includes("REAL") || t.includes("FLOAT") || t.includes("DOUBLE") || t.includes("NUMERIC")) && /^-?\d+(\.\d+)?$/.test(trimmed)) {
      return { kind: "float", value: Number(trimmed) };
    }
    if (t.includes("BOOL") && (trimmed === "true" || trimmed === "false")) {
      return { kind: "bool", value: trimmed === "true" };
    }
    if (t.includes("JSON")) {
      try {
        return { kind: "json", value: JSON.parse(trimmed) };
      } catch {
        // Fallback to plain text if invalid JSON
      }
    }
    return { kind: "text", value: raw };
  }

  const modalOpeners = new Map();
  function openModal(id) {
    const el = document.getElementById(id);
    if (!el) return;
    modalOpeners.set(id, document.activeElement);
    el.hidden = false;
    const focusable = el.querySelector("input, select, textarea, button");
    if (focusable) focusable.focus();
  }

  function closeModal(id) {
    const el = document.getElementById(id);
    if (!el || el.hidden) return;
    el.hidden = true;
    modalOpeners.get(id)?.focus();
    modalOpeners.delete(id);
  }

  function closeAllModals() {
    ["conn-modal", "ddl-modal", "preview-modal", "palette-modal", "discovery-modal"].forEach(closeModal);
  }

  // --- Staged Mutation Logic ---
  function clearStagedChanges() {
    state.insertedRows = [];
    state.updatedCells.clear();
    state.deletedRowIndices.clear();
    state.stagedMutations = [];
    syncStagedBar();
  }

  function rebuildStagedMutations() {
    const ws = state.workspace;
    if (!ws || !ws.data || !ws.active_item) {
      state.stagedMutations = [];
      syncStagedBar();
      return;
    }
    const item = ws.active_item;
    const data = ws.data;
    const pkCols = data.pk_columns && data.pk_columns.length > 0 ? data.pk_columns : [data.columns[0]];
    const mutations = [];

    // 1. Deleted rows
    for (const rowIdx of state.deletedRowIndices) {
      const row = data.rows[rowIdx];
      if (!row) continue;
      const pk_match = {};
      for (const pk of pkCols) {
        const ci = data.columns.indexOf(pk);
        if (ci >= 0) pk_match[pk] = row[ci];
      }
      mutations.push({ op: "delete", item, pk_match });
    }

    // 2. Updated cells grouped by row
    const updatesByRow = new Map();
    for (const [key, val] of state.updatedCells.entries()) {
      const sep = key.indexOf(":");
      const rIdx = Number(key.slice(0, sep));
      const colName = key.slice(sep + 1);
      if (state.deletedRowIndices.has(rIdx)) continue;
      if (!updatesByRow.has(rIdx)) updatesByRow.set(rIdx, {});
      updatesByRow.get(rIdx)[colName] = val;
    }
    for (const [rIdx, changes] of updatesByRow.entries()) {
      const row = data.rows[rIdx];
      if (!row) continue;
      const pk_match = {};
      for (const pk of pkCols) {
        const ci = data.columns.indexOf(pk);
        if (ci >= 0) pk_match[pk] = row[ci];
      }
      mutations.push({ op: "update", item, pk_match, changes });
    }

    // 3. Inserted rows
    for (const newRow of state.insertedRows) {
      const values = {};
      for (const [k, v] of Object.entries(newRow)) {
        if (v) {
          values[k] = v;
        }
      }
      if (Object.keys(values).length > 0) {
        mutations.push({ op: "insert", item, values });
      }
    }

    state.stagedMutations = mutations;
    syncStagedBar();
  }

  function syncStagedBar() {
    const bar = document.getElementById("staged-bar");
    const summary = document.getElementById("staged-summary");
    if (!bar || !summary) return;
    document.getElementById("btn-commit-staged").disabled = !state.stagedMutations.length;
    if (state.stagedMutations.length === 0 && state.insertedRows.length === 0) {
      bar.hidden = true;
      return;
    }
    bar.hidden = false;
    summary.textContent = `${state.stagedMutations.length} perubahan siap di-commit (${state.insertedRows.length} tambah, ${state.updatedCells.size} ubah sel, ${state.deletedRowIndices.size} hapus)`;
  }

  // --- Render Functions ---
  function renderConnections() {
    const select = document.getElementById("connection-select");
    const list = document.getElementById("connection-list");
    const syncSource = document.getElementById("sync-source-select");
    const syncTarget = document.getElementById("sync-target-select");
    const bkpSel = document.getElementById("bkp-conn-select");
    const schedSel = document.getElementById("sched-conn-select");
    const protSel = document.getElementById("protection-mode-select");
    const activeId = state.workspace?.connection?.id || "";

    if (protSel && state.workspace?.connection) {
      const mode = state.workspace.connection.protection_mode || "safe_guard";
      protSel.value = mode;
      protSel.className = `select-control select-protection mode-${mode}`;
    }

    if (select) {
      select.innerHTML = "";
      for (const c of state.connections) {
        const opt = document.createElement("option");
        opt.value = c.id;
        opt.textContent = `${c.name} [${c.environment.toUpperCase()}]`;
        opt.selected = c.id === activeId;
        select.appendChild(opt);
      }
    }

    if (list) {
      list.innerHTML = "";
      for (const c of state.connections) {
        const li = document.createElement("li");
        const btn = document.createElement("button");
        btn.type = "button";
        btn.className = `tree-btn ${c.id === activeId ? "is-active" : ""}`;
        btn.innerHTML = `<span class="tree-label">${escapeHtml(c.name)}</span><span class="tree-meta">${escapeHtml(c.engine)}</span>`;
        btn.addEventListener("click", () => switchConnection(c.id));
        li.appendChild(btn);
        list.appendChild(li);
      }
    }

    const sqlConns = state.connections.filter((c) => ["sqlite", "postgres", "mysql"].includes(c.engine));
    if (syncSource && syncTarget) {
      const prevSrc = syncSource.value;
      const prevTgt = syncTarget.value;
      syncSource.innerHTML = "";
      syncTarget.innerHTML = "";
      for (const c of sqlConns) {
        const o1 = document.createElement("option");
        o1.value = c.id;
        o1.textContent = c.name;
        syncSource.appendChild(o1);

        const o2 = document.createElement("option");
        o2.value = c.id;
        o2.textContent = c.name;
        syncTarget.appendChild(o2);
      }
      syncSource.value = prevSrc || (sqlConns[0]?.id || "");
      syncTarget.value = prevTgt || (sqlConns[1]?.id || sqlConns[0]?.id || "");
    }

    [bkpSel, schedSel].forEach((sel) => {
      if (!sel) return;
      const prev = sel.value;
      sel.innerHTML = "";
      for (const c of state.connections) {
        const opt = document.createElement("option");
        opt.value = c.id;
        opt.textContent = c.name;
        sel.appendChild(opt);
      }
      sel.value = prev || activeId || (state.connections[0]?.id || "");
    });
  }

  function renderWorkspace() {
    const ws = state.workspace;
    if (!ws) return;

    document.getElementById("workspace-path").textContent = ws.connection.name;
    document.getElementById("workspace-object").textContent = ws.active_item || "Ruang kerja";
    const badge = document.getElementById("active-engine-badge");
    if (badge) {
      const envMap = { local: "env-local", staging: "env-staging", production: "env-prod" };
      badge.className = `env-pill ${envMap[ws.connection.environment] || "env-local"}`;
      badge.textContent = `${ws.connection.engine.toUpperCase()} / ${ws.connection.environment.toUpperCase()}`;
    }

    const modeLabel = document.getElementById("editor-mode-label");
    if (modeLabel) {
      if (ws.model === "document") modeLabel.textContent = "MongoDB JSON Filter & Aggregation Console";
      else if (ws.model === "key_value") modeLabel.textContent = "Redis Command Console (GET / SET / HGETALL / INFO)";
      else modeLabel.textContent = "SQL Query Console & Visual Builder";
    }

    const sqlEditor = document.getElementById("sql-editor");
    if (sqlEditor && !sqlEditor.dataset.userEdited) {
      if (ws.model === "document") {
        sqlEditor.value = '{\n  "severity": "error"\n}';
      } else if (ws.model === "key_value") {
        sqlEditor.value = "KEYS *";
      } else if (ws.active_item) {
        sqlEditor.value = `SELECT * FROM "${ws.active_item}" LIMIT 100;`;
      }
    }

    renderObjectTree();
    renderVisualBuilderOptions();
    renderDataGrid();
    renderStructure();
    renderErdSvg();
    renderSavedQueries();
  }

  function renderObjectTree() {
    const ws = state.workspace;
    const tree = document.getElementById("object-tree");
    const countBadge = document.getElementById("objects-count");
    const filterVal = (document.getElementById("sidebar-search")?.value || "").trim().toLowerCase();
    if (!ws || !tree) return;

    const filtered = ws.items.filter((i) => i.name.toLowerCase().includes(filterVal));
    if (countBadge) countBadge.textContent = String(filtered.length);
    tree.innerHTML = "";

    if (filtered.length === 0) {
      const empty = document.createElement("li");
      empty.className = "tree-item-placeholder";
      empty.textContent = "Tidak ada tabel atau koleksi yang cocok.";
      tree.appendChild(empty);
      return;
    }

    for (const item of filtered) {
      const li = document.createElement("li");
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = `tree-btn ${item.name === ws.active_item ? "is-active" : ""}`;
      btn.innerHTML = `<span class="tree-label">${escapeHtml(item.name)}</span><span class="tree-meta">${item.row_count} ${escapeHtml(item.item_type)}</span>`;
      btn.addEventListener("click", () => {
        const sqlEditor = document.getElementById("sql-editor");
        if (sqlEditor) delete sqlEditor.dataset.userEdited;
        loadWorkspaceItem(item.name);
        // Close mobile drawer after selection
        const sb = document.getElementById("object-sidebar");
        const tg = document.getElementById("btn-toggle-sidebar");
        if (sb && sb.classList.contains("is-open")) {
          sb.classList.remove("is-open");
          if (tg) tg.setAttribute("aria-expanded", "false");
        }
      });
      li.appendChild(btn);
      tree.appendChild(li);
    }
  }

  function renderVisualBuilderOptions() {
    const ws = state.workspace;
    const qbTable = document.getElementById("qb-table");
    const qbJoin = document.getElementById("qb-join-table");
    if (!ws || !qbTable || !qbJoin) return;
    qbTable.innerHTML = "";
    qbJoin.innerHTML = '<option value="">Tanpa JOIN</option>';
    for (const item of ws.items) {
      const o1 = document.createElement("option");
      o1.value = item.name;
      o1.textContent = item.name;
      if (item.name === ws.active_item) o1.selected = true;
      qbTable.appendChild(o1);

      const o2 = document.createElement("option");
      o2.value = item.name;
      o2.textContent = item.name;
      qbJoin.appendChild(o2);
    }
  }

  function renderDataGrid() {
    const ws = state.workspace;
    const titleEl = document.getElementById("active-item-title");
    const metaEl = document.getElementById("query-meta-text");
    const thead = document.getElementById("data-thead");
    const tbody = document.getElementById("data-tbody");
    const stateBox = document.getElementById("data-state-box");
    const docView = document.getElementById("document-cards-view");
    if (!ws || !thead || !tbody) return;

    const table = ws.items.find(i => i.name === ws.active_item)?.item_type === "TABLE";
    const writable = !state.queryResult && ws.active_item && ws.connection.protection_mode !== "read_only" && (ws.model !== "relational" || table);
    const paged = !state.queryResult && ws.model === "relational" && Boolean(ws.data);
    document.getElementById("grid-pages").hidden = !paged;
    document.getElementById("btn-add-row").disabled = !writable;
    const relational = ws.model === "relational";
    const canCreate = relational && ws.connection.protection_mode !== "read_only";
    document.getElementById("grid-insert-table").disabled = !canCreate;
    document.getElementById("grid-insert-column").disabled = !relational || !table || !writable;
    document.getElementById("grid-copy-table").disabled = !table || state.queryResult;
    const paste = document.getElementById("grid-paste-table");
    paste.disabled = !canCreate || state.copiedTable?.connection !== ws.connection.id || state.pasting;
    paste.title = state.copiedTable ? `Sumber: ${state.copiedTable.name} (${state.copiedTable.connectionName})` : "Salin tabel terlebih dahulu";
    if (paged) {
      const count = ws.data.rows.length;
      document.getElementById("grid-range").textContent = count ? `${state.offset + 1}–${state.offset + count} dari ${ws.data.total_rows}` : "0 baris";
      document.getElementById("grid-prev").disabled = !state.offset;
      document.getElementById("grid-next").disabled = state.offset + count >= ws.data.total_rows;
    }
    if (titleEl) titleEl.textContent = state.queryResult ? "Hasil query" : ws.active_item || "Hasil query";
    const data = ws.data;
    if (!data) {
      thead.innerHTML = "";
      tbody.innerHTML = "";
      if (docView) docView.hidden = true;
      if (stateBox) {
        stateBox.hidden = false;
        stateBox.textContent = "Pilih tabel atau jalankan query untuk melihat data.";
      }
      if (metaEl) metaEl.textContent = "0 baris";
      return;
    }

    if (metaEl) {
      const totalLabel = data.total_rows !== null && data.total_rows !== undefined ? `${data.total_rows} total` : `${data.rows.length} baris`;
      metaEl.textContent = `${data.rows.length} tampil (${totalLabel}) | ${data.duration_ms} ms`;
    }

    // Header
    thead.innerHTML = "";
    const trHead = document.createElement("tr");
    const thAct = document.createElement("th");
    thAct.textContent = "Aksi";
    trHead.appendChild(thAct);

    data.columns.forEach((col, i) => {
      const th = document.createElement("th");
      const sortBtn = document.createElement("button");
      sortBtn.type = "button";
      sortBtn.className = "th-sort-btn";
      const colType = data.column_types[i] || "";
      const arrow = state.sortCol === col ? (state.sortDir === "asc" ? " [ASC]" : " [DESC]") : "";
      sortBtn.innerHTML = `<span>${escapeHtml(col)}${arrow}</span><span class="col-type-tag">${escapeHtml(colType)}</span>`;
      sortBtn.addEventListener("click", () => toggleSort(col));
      th.appendChild(sortBtn);
      trHead.appendChild(th);
    });
    thead.appendChild(trHead);

    // Body
    tbody.innerHTML = "";
    const hasRows = data.rows.length > 0 || state.insertedRows.length > 0;
    if (stateBox) {
      stateBox.hidden = hasRows;
      if (!hasRows) {
        stateBox.textContent = data.notice || "Tabel atau hasil query ini kosong (0 baris). Klik '+ Tambah Baris' untuk menyisipkan data.";
      }
    }

    // Inserted rows at top
    state.insertedRows.forEach((insObj, insIdx) => {
      const tr = document.createElement("tr");
      tr.className = "row-inserted";
      const tdAct = document.createElement("td");
      const chip = document.createElement("span");
      chip.className = "row-state-chip chip-insert";
      chip.textContent = "+ INSERT";
      tdAct.appendChild(chip);
      const rmBtn = document.createElement("button");
      rmBtn.type = "button";
      rmBtn.className = "btn btn-sm btn-ghost";
      rmBtn.textContent = "Batal";
      rmBtn.addEventListener("click", () => {
        state.insertedRows.splice(insIdx, 1);
        rebuildStagedMutations();
        renderDataGrid();
      });
      tdAct.appendChild(rmBtn);
      tr.appendChild(tdAct);

      data.columns.forEach((col, cIdx) => {
        const td = document.createElement("td");
        const cell = insObj[col] || { kind: "null" };
        const inp = document.createElement("input");
        inp.type = "text";
        inp.className = "cell-input";
        inp.setAttribute("aria-label", `Baris baru kolom ${col}`);
        inp.value = cell.kind === "null" ? "" : cellToDisplay(cell);
        inp.placeholder = col;
        inp.addEventListener("input", () => {
          if (inp.value === "") delete insObj[col];
          else insObj[col] = parseInputToCell(inp.value, data.column_types[cIdx]);
          rebuildStagedMutations();
        });
        td.appendChild(inp);
        tr.appendChild(td);
      });
      tbody.appendChild(tr);
    });

    // Existing rows
    data.rows.forEach((row, rIdx) => {
      const tr = document.createElement("tr");
      const isDeleted = state.deletedRowIndices.has(rIdx);
      let rowUpdated = false;
      data.columns.forEach((c) => {
        if (state.updatedCells.has(`${rIdx}:${c}`)) rowUpdated = true;
      });
      if (isDeleted) tr.className = "row-deleted";
      else if (rowUpdated) tr.className = "row-updated";

      const tdAct = document.createElement("td");
      if (isDeleted) {
        const chip = document.createElement("span");
        chip.className = "row-state-chip chip-delete";
        chip.textContent = "- DELETE";
        tdAct.appendChild(chip);
      } else if (rowUpdated) {
        const chip = document.createElement("span");
        chip.className = "row-state-chip chip-update";
        chip.textContent = "~ UPDATE";
        tdAct.appendChild(chip);
      }

      if (writable && data.pk_columns?.length) {
        const delBtn = document.createElement("button");
        delBtn.type = "button";
        delBtn.className = "btn btn-sm btn-ghost";
        delBtn.textContent = isDeleted ? "Pulihkan" : "Hapus";
        delBtn.setAttribute("aria-label", `${isDeleted ? "Pulihkan" : "Hapus"} baris ${rIdx + 1}`);
        delBtn.addEventListener("click", () => {
          if (state.deletedRowIndices.has(rIdx)) state.deletedRowIndices.delete(rIdx);
          else state.deletedRowIndices.add(rIdx);
          rebuildStagedMutations();
          renderDataGrid();
        });
        delBtn.disabled = ws.connection.protection_mode !== "unrestricted";
        if (delBtn.disabled) delBtn.title = "Pilih Unrestricted untuk menghapus baris";
        tdAct.appendChild(delBtn);
        if (table && !isDeleted) {
          const copy = document.createElement("button");
          copy.type = "button";
          copy.className = "btn btn-sm btn-ghost";
          copy.textContent = "Duplikat";
          copy.setAttribute("aria-label", `Duplikat baris ${rIdx + 1}`);
          copy.onclick = () => {
            state.insertedRows.unshift(Object.fromEntries(data.columns.flatMap((col, idx) =>
              data.pk_columns.includes(col) ? [] : [[col, state.updatedCells.get(`${rIdx}:${col}`) ?? row[idx]]])));
            rebuildStagedMutations();
            renderDataGrid();
          };
          tdAct.appendChild(copy);
        }
      } else {
        const ro = document.createElement("span");
        ro.className = "meta-text";
        ro.textContent = `#${rIdx + 1}`;
        tdAct.appendChild(ro);
      }
      tr.appendChild(tdAct);

      row.forEach((origCell, cIdx) => {
        const colName = data.columns[cIdx];
        const colType = data.column_types[cIdx];
        const key = `${rIdx}:${colName}`;
        const stagedCell = state.updatedCells.get(key);
        const displayCell = stagedCell !== undefined ? stagedCell : origCell;
        const td = document.createElement("td");

        const canEdit = Boolean(writable && data.pk_columns?.length && !isDeleted && origCell.kind !== "bytes");
        if (!canEdit) {
          const span = document.createElement("span");
          span.className = displayCell.kind === "null" ? "cell-null" : "";
          span.textContent = cellToDisplay(displayCell);
          td.appendChild(span);
        } else {
          const btn = document.createElement("button");
          btn.type = "button";
          btn.className = `cell-btn ${displayCell.kind === "null" ? "cell-null" : ""}`;
          btn.textContent = cellToDisplay(displayCell);
          btn.setAttribute("aria-label", `Edit ${colName} baris ${rIdx + 1}`);
          btn.addEventListener("click", () => startInlineEdit(td, rIdx, colName, colType, origCell, displayCell));
          td.appendChild(btn);
        }
        tr.appendChild(td);
      });
      tbody.appendChild(tr);
    });

    // MongoDB Document Tree Cards view
    if (docView) {
      if (ws.model === "document" && data.rows.length > 0) {
        docView.hidden = false;
        docView.innerHTML = "";
        data.rows.forEach((row, idx) => {
          const doc = {};
          data.columns.forEach((c, ci) => {
            const cell = row[ci];
            doc[c] = cell?.kind === "null" ? null : cell?.value;
          });
          const card = document.createElement("div");
          card.className = "doc-card";
          card.innerHTML = `
            <div class="doc-card-head">
              <strong>Dokumen #${idx + 1} (${escapeHtml(String(doc._id || ""))})</strong>
              <span class="meta-text">JSON / BSON</span>
            </div>
            <pre class="code-block">${escapeHtml(JSON.stringify(doc, null, 2))}</pre>
          `;
          docView.appendChild(card);
        });
      } else {
        docView.hidden = true;
        docView.innerHTML = "";
      }
    }
  }

  function startInlineEdit(td, rIdx, colName, colType, origCell, currentCell) {
    td.innerHTML = "";
    const inp = document.createElement("input");
    inp.type = "text";
    inp.className = "cell-input";
    inp.value = currentCell.kind === "null" ? "NULL" : cellToDisplay(currentCell);
    inp.setAttribute("aria-label", `Ubah nilai ${colName}`);

    let finished = false;
    const clear = document.createElement("button");
    clear.type = "button";
    clear.className = "btn btn-sm btn-ghost";
    clear.textContent = "Set NULL";
    clear.setAttribute("aria-label", `Set NULL pada ${colName}`);
    const commitLocal = (parsed) => {
      if (finished) return;
      finished = true;
      const key = `${rIdx}:${colName}`;
      if (JSON.stringify(parsed) === JSON.stringify(origCell)) state.updatedCells.delete(key);
      else state.updatedCells.set(key, parsed);
      rebuildStagedMutations();
      renderDataGrid();
    };
    const value = () => inp.value === cellToDisplay(currentCell) ? currentCell :
      parseInputToCell(inp.value, colType, false);
    clear.onclick = () => commitLocal({ kind: "null" });
    td.addEventListener("focusout", ev => {
      if (!td.contains(ev.relatedTarget)) commitLocal(value());
    });
    inp.onkeydown = ev => {
      if (ev.key === "Enter") { ev.preventDefault(); commitLocal(value()); }
      if (ev.key === "Escape") { ev.preventDefault(); finished = true; renderDataGrid(); }
    };
    td.append(inp, clear);
    inp.focus();
    inp.select();
  }

  function renderStructure() {
    const st = state.workspace?.structure;
    const isTable = state.workspace?.items.some(i => i.name === st?.item_name && i.item_type === "TABLE");
    document.getElementById("rename-table").hidden = !isTable;
    document.getElementById("rename-table").disabled = state.workspace?.connection.protection_mode === "read_only";
    const title = document.getElementById("structure-title");
    const colsBody = document.getElementById("structure-cols-body");
    const idxList = document.getElementById("structure-indexes-list");
    const fkList = document.getElementById("structure-fks-list");
    const ddlPre = document.getElementById("structure-ddl-code");
    if (!colsBody || !idxList || !fkList || !ddlPre) return;

    if (!st) {
      if (title) title.textContent = "Struktur Objek";
      colsBody.innerHTML = "";
      idxList.innerHTML = "<li>Tidak ada indeks</li>";
      fkList.innerHTML = "<li>Tidak ada foreign key</li>";
      ddlPre.textContent = "";
      return;
    }

    if (title) title.textContent = `Struktur: ${st.item_name} (${st.item_type})`;
    colsBody.innerHTML = "";
    for (const c of st.columns) {
      const tr = document.createElement("tr");
      tr.innerHTML = `
        <td><code>${escapeHtml(c.name)}</code></td>
        <td><code>${escapeHtml(c.data_type)}</code></td>
        <td>${c.is_pk ? "<strong>PK</strong>" : "-"}</td>
        <td>${c.nullable ? "YES" : "NOT NULL"}</td>
        <td><code>${escapeHtml(c.default_value || "-")}</code></td>
      `;
      const actions = tr.insertCell();
      if (isTable) {
        for (const [action, label] of [["rename_column", "Ubah nama"], ["drop_column", "Hapus"]]) {
          const btn = document.createElement("button");
          btn.type = "button";
          btn.className = "btn btn-sm btn-ghost";
          btn.textContent = label;
          btn.setAttribute("aria-label", `${label} kolom ${c.name}`);
          btn.disabled = state.workspace.connection.protection_mode === "read_only" || (action === "drop_column" && state.workspace.connection.protection_mode !== "unrestricted");
          btn.onclick = () => changeSchema(action, c.name);
          actions.appendChild(btn);
        }
      }
      colsBody.appendChild(tr);
    }

    idxList.innerHTML = st.indexes.length
      ? st.indexes.map((i) => `<li><code>${escapeHtml(i.name)}</code> (${i.unique ? "UNIQUE" : "INDEX"}: ${escapeHtml(i.columns.join(", ") || "-")})</li>`).join("")
      : "<li>Belum ada indeks tambahan.</li>";

    fkList.innerHTML = st.foreign_keys.length
      ? st.foreign_keys.map((f) => `<li><code>${escapeHtml(f.from_column)}</code> -&gt; <code>${escapeHtml(f.to_table)}.${escapeHtml(f.to_column)}</code> (ON DELETE ${escapeHtml(f.on_delete)})</li>`).join("")
      : "<li>Tidak ada relasi Foreign Key.</li>";

    ddlPre.textContent = st.ddl || "-- DDL tidak tersedia untuk tipe objek ini";
  }

  function renderErdSvg() {
    const wrap = document.getElementById("erd-canvas-wrap");
    const stats = document.getElementById("erd-stats");
    const erd = state.workspace?.erd;
    if (!wrap) return;

    if (!erd || !erd.nodes || erd.nodes.length === 0) {
      if (stats) stats.textContent = "Model non-relasional atau belum ada tabel.";
      wrap.innerHTML = '<p class="meta-text">Diagram ERD tersedia untuk koneksi relasional (SQLite, PostgreSQL, MySQL/MariaDB).</p>';
      return;
    }

    if (stats) stats.textContent = `${erd.nodes.length} tabel, ${erd.edges.length} relasi Foreign Key`;
    const colW = 240;
    const gapX = 56;
    const gapY = 32;
    const colsPerRow = 3;
    const positions = new Map();
    let maxH = 0;

    // Pre-calculate box coordinates
    const rowHeights = [];
    erd.nodes.forEach((node, idx) => {
      const row = Math.floor(idx / colsPerRow);
      const h = 42 + node.columns.length * 22 + 12;
      rowHeights[row] = Math.max(rowHeights[row] || 0, h);
    });

    const rowTops = [20];
    for (let r = 1; r < rowHeights.length; r++) {
      rowTops[r] = rowTops[r - 1] + rowHeights[r - 1] + gapY;
    }

    erd.nodes.forEach((node, idx) => {
      const col = idx % colsPerRow;
      const row = Math.floor(idx / colsPerRow);
      const x = 20 + col * (colW + gapX);
      const y = rowTops[row];
      const h = 42 + node.columns.length * 22 + 12;
      positions.set(node.table_name, { x, y, w: colW, h });
      maxH = Math.max(maxH, y + h + 24);
    });

    const svgW = Math.max(680, Math.min(erd.nodes.length, colsPerRow) * (colW + gapX) + 20);
    let edgesSvg = "";
    for (const e of erd.edges) {
      const src = positions.get(e.from_table);
      const dst = positions.get(e.to_table);
      if (!src || !dst) continue;
      const x1 = src.x + src.w;
      const y1 = src.y + 30;
      const x2 = dst.x;
      const y2 = dst.y + 30;
      const cx = (x1 + x2) / 2;
      edgesSvg += `<path d="M ${x1} ${y1} C ${cx} ${y1}, ${cx} ${y2}, ${x2} ${y2}" fill="none" stroke="var(--accent-link)" stroke-width="2" marker-end="url(#fk-arrow)" />`;
    }

    let nodesSvg = "";
    for (const node of erd.nodes) {
      const pos = positions.get(node.table_name);
      let colsMarkup = "";
      node.columns.forEach((c, idx) => {
        const cy = pos.y + 54 + idx * 22;
        const badge = c.is_pk ? "PK " : "";
        colsMarkup += `<text x="${pos.x + 12}" y="${cy}" fill="var(--text)" font-size="12" font-family="ui-monospace, monospace">${escapeHtml(badge + c.name)}</text>`;
        colsMarkup += `<text x="${pos.x + pos.w - 12}" y="${cy}" text-anchor="end" fill="var(--text-muted)" font-size="11" font-family="ui-monospace, monospace">${escapeHtml(c.data_type)}</text>`;
      });

      nodesSvg += `
        <g>
          <rect x="${pos.x}" y="${pos.y}" width="${pos.w}" height="${pos.h}" rx="6" fill="var(--surface)" stroke="var(--control-border)" stroke-width="1.5" />
          <rect x="${pos.x}" y="${pos.y}" width="${pos.w}" height="32" rx="6" fill="var(--surface-muted)" />
          <text x="${pos.x + 12}" y="${pos.y + 21}" fill="var(--text)" font-size="13" font-weight="700">${escapeHtml(node.table_name)} (${node.row_count})</text>
          ${colsMarkup}
        </g>
      `;
    }

    wrap.innerHTML = `
      <svg width="${svgW}" height="${maxH}" viewBox="0 0 ${svgW} ${maxH}" role="img" aria-label="Diagram Relasi Tabel ERD">
        <defs>
          <marker id="fk-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
            <path d="M 0 1 L 10 5 L 0 9 z" fill="var(--accent-link)" />
          </marker>
        </defs>
        ${edgesSvg}
        ${nodesSvg}
      </svg>
    `;
  }

  function renderHistory() {
    const tbody = document.getElementById("history-tbody");
    if (!tbody) return;
    tbody.innerHTML = "";
    if (!state.history || state.history.length === 0) {
      const tr = document.createElement("tr");
      tr.innerHTML = '<td colspan="6" class="meta-text">Belum ada query yang dieksekusi pada sesi ini.</td>';
      tbody.appendChild(tr);
      return;
    }
    for (const h of state.history) {
      const tr = document.createElement("tr");
      tr.innerHTML = `
        <td>${escapeHtml(h.executed_at.replace("T", " ").slice(0, 19))}</td>
        <td>${escapeHtml(h.connection_name)}</td>
        <td><code>${escapeHtml(h.statement)}</code></td>
        <td>${escapeHtml(h.status)}</td>
        <td>${h.affected_rows}</td>
        <td>${h.duration_ms} ms</td>
      `;
      tbody.appendChild(tr);
    }
  }

  function renderSavedQueries() {
    const sel = document.getElementById("saved-query-select");
    if (!sel) return;
    sel.innerHTML = '<option value="">Pilih Query Tersimpan...</option>';
    for (const q of state.savedQueries) {
      const opt = document.createElement("option");
      opt.value = q.id;
      opt.textContent = q.title;
      sel.appendChild(opt);
    }
  }

  // --- API Actions ---
  function canLeaveGrid() {
    return !state.saving && ((!state.insertedRows.length && !state.stagedMutations.length) || confirm("Buang perubahan yang belum disimpan?"));
  }

  async function changeSchema(action, column) {
    const ws = state.workspace;
    if (!ws || state.stagedMutations.length || state.insertedRows.length) {
      showBanner("Simpan atau batalkan perubahan grid terlebih dahulu.", "error");
      return;
    }
    const payload = { action, table_name: ws.active_item };
    if (column) payload.column_name = column;
    if (action === "drop_column") {
      if (!confirm(`Hapus kolom ${column} beserta seluruh nilainya dari ${ws.active_item}?`)) return;
    } else {
      const name = prompt("Nama baru", column || ws.active_item)?.trim();
      if (!name || name === (column || ws.active_item)) return;
      payload.new_name = name;
    }
    try {
      await apiJson(`/api/connections/${encodeURIComponent(ws.connection.id)}/ddl`, { method: "POST", body: JSON.stringify(payload) });
      if (state.workspace === ws) await loadWorkspaceItem(action === "rename_table" ? payload.new_name : ws.active_item);
    } catch (err) { showBanner(err.message, "error"); }
  }

  async function switchConnection(connId) {
    if (!connId || !canLeaveGrid()) { renderConnections(); return; }
    const requestId = ++state.requestId;
    clearBanner();
    clearStagedChanges();
    state.sortCol = null;
    state.filterText = "";
    const filterInput = document.getElementById("quick-filter-input");
    if (filterInput) filterInput.value = "";
    const sqlEditor = document.getElementById("sql-editor");
    if (sqlEditor) delete sqlEditor.dataset.userEdited;

    try {
      const ws = await apiJson(`/api/connections/${encodeURIComponent(connId)}/workspace`);
      if (requestId !== state.requestId) return;
      state.offset = 0;
      state.limit = 100;
      document.getElementById("grid-limit").value = "100";
      state.queryResult = false;
      state.workspace = ws;
      renderConnections();
      renderWorkspace();
    } catch (err) {
      showBanner(`Gagal membuka koneksi: ${err.message}`, "error");
    }
  }

  async function loadWorkspaceItem(itemName, offset = 0, limit = state.limit) {
    if (!state.workspace || !canLeaveGrid()) return;
    const requestId = ++state.requestId;
    clearBanner();
    clearStagedChanges();
    const connId = state.workspace.connection.id;
    const params = new URLSearchParams({offset, limit});
    if (itemName) params.set("item", itemName);
    if (state.filterText) params.set("filter", state.filterText);
    if (state.sortCol) {
      params.set("sort_col", state.sortCol);
      params.set("sort_dir", state.sortDir);
    }
    try {
      const ws = await apiJson(`/api/connections/${encodeURIComponent(connId)}/workspace?${params.toString()}`);
      if (requestId !== state.requestId) return;
      state.offset = offset;
      state.limit = limit;
      state.queryResult = false;
      state.workspace = ws;
      renderWorkspace();
    } catch (err) {
      showBanner(`Gagal memuat objek: ${err.message}`, "error");
    }
  }

  function toggleSort(col) {
    if (state.sortCol === col) {
      state.sortDir = state.sortDir === "asc" ? "desc" : "asc";
    } else {
      state.sortCol = col;
      state.sortDir = "asc";
    }
    if (state.workspace?.active_item) {
      loadWorkspaceItem(state.workspace.active_item);
    }
  }

  async function runQuery(overrideSql) {
    if (!state.workspace || !canLeaveGrid()) return;
    const requestId = ++state.requestId;
    clearBanner();
    const editor = document.getElementById("sql-editor");
    const query = (overrideSql !== undefined ? overrideSql : editor?.value || "").trim();
    if (!query) {
      showBanner("Query tidak boleh kosong.", "error");
      return;
    }
    try {
      const res = await apiJson(`/api/connections/${encodeURIComponent(state.workspace.connection.id)}/query`, {
        method: "POST",
        body: JSON.stringify({ item: state.workspace.active_item, query }),
      });
      if (requestId !== state.requestId) return;
      state.queryResult = true;
      clearStagedChanges();
      state.workspace.data = res;
      switchTab("data");
      renderDataGrid();
      await refreshHistory();
      showBanner(`Query selesai dalam ${res.duration_ms} ms (${res.rows.length} baris).`, "success");
    } catch (err) {
      await refreshHistory();
      showBanner(`Error query: ${err.message}`, "error");
    }
  }

  async function commitStaged() {
    if (state.saving || !state.workspace || state.stagedMutations.length === 0) return;
    state.saving = true;
    document.getElementById("main-workspace").inert = true;
    clearBanner();
    try {
      const report = await apiJson(`/api/connections/${encodeURIComponent(state.workspace.connection.id)}/mutate`, {
        method: "POST",
        body: JSON.stringify({ mutations: state.stagedMutations }),
      });
      closeModal("preview-modal");
      clearStagedChanges();
      state.saving = false;
      await loadWorkspaceItem(state.workspace.active_item, state.offset);
      await refreshHistory();
      showBanner(
        `Berhasil commit ${report.committed_count} perubahan dalam satu transaksi (${report.duration_ms} ms).`,
        "success"
      );
    } catch (err) {
      showBanner(`Gagal commit transaksi: ${err.message}`, "error");
    } finally {
      state.saving = false;
      document.getElementById("main-workspace").inert = false;
    }
  }

  async function refreshHistory() {
    try {
      state.history = await apiJson("/api/history");
      renderHistory();
    } catch {
      // Ignore background history refresh failure
    }
  }

  async function compareSchemas() {
    const src = document.getElementById("sync-source-select")?.value;
    const tgt = document.getElementById("sync-target-select")?.value;
    const box = document.getElementById("sync-results-box");
    const applyBtn = document.getElementById("btn-apply-sync");
    if (!src || !tgt || !box) return;

    try {
      const res = await apiJson("/api/schema-sync/diff", {
        method: "POST",
        body: JSON.stringify({ source_conn_id: src, target_conn_id: tgt }),
      });
      state.lastSyncDiff = { source_conn_id: src, target_conn_id: tgt, res };
      if (res.differences.length === 0) {
        box.innerHTML = '<div class="status-banner is-success">Skema identik. Tidak ada perbedaan antara Sumber dan Target.</div>';
        if (applyBtn) applyBtn.disabled = true;
        return;
      }
      if (applyBtn) applyBtn.disabled = false;
      const itemsHtml = res.differences
        .map(
          (d) => `
          <div class="sync-diff-item">
            <strong>${escapeHtml(d.object_name)} (${escapeHtml(d.kind)})</strong>
            <p>${escapeHtml(d.detail)}</p>
            <pre class="code-block">${escapeHtml(d.migration_sql)}</pre>
          </div>`
        )
        .join("");
      box.innerHTML = `
        <p><strong>${res.differences.length} perbedaan skema ditemukan</strong> (${escapeHtml(res.source_name)} -&gt; ${escapeHtml(res.target_name)})</p>
        <div class="sync-diff-list">${itemsHtml}</div>
      `;
    } catch (err) {
      box.innerHTML = `<div class="status-banner is-error">${escapeHtml(err.message)}</div>`;
    }
  }

  async function applySchemaSync() {
    if (!state.lastSyncDiff) return;
    const { source_conn_id, target_conn_id, res } = state.lastSyncDiff;
    const statements = res.differences.map((d) => d.migration_sql);
    const box = document.getElementById("sync-results-box");
    const applyBtn = document.getElementById("btn-apply-sync");
    try {
      const out = await apiJson("/api/schema-sync/apply", {
        method: "POST",
        body: JSON.stringify({ source_conn_id, target_conn_id, statements }),
      });
      if (applyBtn) applyBtn.disabled = out.remaining_differences === 0;
      await refreshHistory();
      if (box) {
        box.innerHTML = `<div class="status-banner is-success">${out.applied_statements} pernyataan DDL diterapkan. Sisa perbedaan: ${out.remaining_differences}.</div>`;
      }
    } catch (err) {
      if (box) box.innerHTML = `<div class="status-banner is-error">${escapeHtml(err.message)}</div>`;
    }
  }

  function switchTab(tabName) {
    state.activeTab = tabName;
    const tabs = ["data", "structure", "erd", "sync", "backup", "history"];
    for (const t of tabs) {
      const btn = document.getElementById(`tab-btn-${t}`);
      const panel = document.getElementById(`panel-${t}`);
      const active = t === tabName;
      if (btn) {
        btn.classList.toggle("is-active", active);
        btn.setAttribute("aria-selected", active ? "true" : "false");
      }
      if (panel) panel.hidden = !active;
    }
    if (tabName === "history") refreshHistory();
    if (tabName === "backup") loadBackupsDashboard();
  }

  function escapeHtml(str) {
    return String(str)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  // --- Wire Events ---
  function bindEvents() {
    // Theme toggle
    const themeBtn = document.getElementById("btn-theme-toggle");
    const themeLbl = document.getElementById("theme-toggle-label");
    const syncThemeLabel = () => {
      const cur = document.documentElement.getAttribute("data-theme") || "light";
      if (themeLbl) themeLbl.textContent = cur === "dark" ? "Mode Terang" : "Mode Gelap";
      if (themeBtn) themeBtn.setAttribute("aria-pressed", cur === "dark" ? "true" : "false");
    };
    syncThemeLabel();
    themeBtn?.addEventListener("click", () => {
      const cur = document.documentElement.getAttribute("data-theme") || "light";
      const next = cur === "dark" ? "light" : "dark";
      document.documentElement.setAttribute("data-theme", next);
      try {
        localStorage.setItem("guidb_theme", next);
      } catch {
        // Ignore storage restrictions
      }
      syncThemeLabel();
    });

    // Mobile sidebar toggle
    const toggleSidebarBtn = document.getElementById("btn-toggle-sidebar");
    const sidebar = document.getElementById("object-sidebar");
    toggleSidebarBtn?.addEventListener("click", () => {
      if (!sidebar) return;
      const open = sidebar.classList.toggle("is-open");
      toggleSidebarBtn.setAttribute("aria-expanded", open ? "true" : "false");
    });

    document.getElementById("connection-select")?.addEventListener("change", (ev) => {
      switchConnection(ev.target.value);
    });

    document.getElementById("protection-mode-select")?.addEventListener("change", async (ev) => {
      const conn = state.workspace?.connection;
      if (!conn) return;
      const nextMode = ev.target.value;
      try {
        const updated = await apiJson(`/api/connections/${encodeURIComponent(conn.id)}/protection`, {
          method: "PUT",
          body: JSON.stringify({ protection_mode: nextMode }),
        });
        if (state.workspace.connection.id !== updated.id) return;
        state.workspace.connection = updated;
        renderWorkspace();
        const idx = state.connections.findIndex((c) => c.id === updated.id);
        if (idx >= 0) state.connections[idx] = updated;
        renderConnections();
        showBanner(`Mode proteksi '${updated.name}' diubah ke ${nextMode.toUpperCase()}.`, "success");
      } catch (err) {
        showBanner(`Gagal mengubah proteksi: ${err.message}`, "error");
      }
    });

    document.getElementById("btn-quick-backup")?.addEventListener("click", () => {
      switchTab("backup");
    });

    document.getElementById("btn-refresh-workspace")?.addEventListener("click", () => {
      loadWorkspaceItem(state.workspace?.active_item || "");
    });

    document.getElementById("sidebar-search")?.addEventListener("input", renderObjectTree);

    // Tabs
    ["data", "structure", "erd", "sync", "backup", "history"].forEach((t) => {
      document.getElementById(`tab-btn-${t}`)?.addEventListener("click", () => switchTab(t));
    });

    // Backups & Schedules
    document.getElementById("btn-refresh-backups")?.addEventListener("click", loadBackupsDashboard);
    document.getElementById("form-create-backup")?.addEventListener("submit", async (ev) => {
      ev.preventDefault();
      const connection_id = document.getElementById("bkp-conn-select")?.value;
      const scope = document.getElementById("bkp-scope-select")?.value || "full_schema_and_data";
      const label = document.getElementById("bkp-label-input")?.value.trim() || null;
      if (!connection_id) return;
      try {
        const rec = await apiJson("/api/backups/create", {
          method: "POST",
          body: JSON.stringify({ connection_id, scope, label }),
        });
        const lblInp = document.getElementById("bkp-label-input");
        if (lblInp) lblInp.value = "";
        await loadBackupsDashboard();
        await refreshHistory();
        showBanner(`Backup manual '${rec.file_name}' dibuat (SHA-256: ${rec.sha256.slice(0, 12)}...).`, "success");
      } catch (err) {
        showBanner(`Gagal membuat backup: ${err.message}`, "error");
      }
    });

    document.getElementById("form-create-schedule")?.addEventListener("submit", async (ev) => {
      ev.preventDefault();
      const connection_id = document.getElementById("sched-conn-select")?.value;
      const interval_minutes = Number(document.getElementById("sched-interval-select")?.value || 60);
      const retention_count = Number(document.getElementById("sched-retention-input")?.value || 7);
      const scope = document.getElementById("sched-scope-select")?.value || "full_schema_and_data";
      if (!connection_id) return;
      try {
        const sched = await apiJson("/api/backups/schedules", {
          method: "POST",
          body: JSON.stringify({ connection_id, interval_minutes, scope, retention_count }),
        });
        await loadBackupsDashboard();
        showBanner(`Jadwal backup otomatis tiap ${sched.interval_minutes} menit untuk '${sched.connection_name}' disimpan.`, "success");
      } catch (err) {
        showBanner(`Gagal membuat jadwal: ${err.message}`, "error");
      }
    });

    // Query Console
    document.getElementById("sql-editor")?.addEventListener("input", (ev) => {
      ev.target.dataset.userEdited = "1";
    });
    document.getElementById("btn-run-query")?.addEventListener("click", () => runQuery());
    document.getElementById("btn-format-sql")?.addEventListener("click", () => {
      const ed = document.getElementById("sql-editor");
      if (!ed) return;
      ed.value = wasmFormatSql(ed.value);
      ed.dataset.userEdited = "1";
      showBanner("Query diformat menggunakan engine Rust WebAssembly.", "success");
    });
    document.getElementById("btn-explain-query")?.addEventListener("click", () => {
      const ed = document.getElementById("sql-editor");
      if (!ed || !state.workspace) return;
      if (state.workspace.model !== "relational") {
        showBanner("Explain Plan tersedia untuk query relasional SQL.", "info");
        return;
      }
      const prefix = state.workspace.connection.engine === "sqlite" ? "EXPLAIN QUERY PLAN\n" : "EXPLAIN\n";
      const q = ed.value.trim();
      const targetSql = q.toUpperCase().startsWith("EXPLAIN") ? q : prefix + q;
      runQuery(targetSql);
    });

    document.getElementById("saved-query-select")?.addEventListener("change", (ev) => {
      const found = state.savedQueries.find((q) => q.id === ev.target.value);
      if (!found) return;
      const ed = document.getElementById("sql-editor");
      if (ed) {
        ed.value = found.statement;
        ed.dataset.userEdited = "1";
      }
      if (found.connection_id && state.workspace?.connection?.id !== found.connection_id) {
        switchConnection(found.connection_id);
      }
    });

    document.getElementById("btn-save-query")?.addEventListener("click", async () => {
      const ed = document.getElementById("sql-editor");
      const stmt = (ed?.value || "").trim();
      if (!stmt || !state.workspace) return;
      const title = `${state.workspace.active_item || "Query"} (${new Date().toLocaleTimeString("id-ID")})`;
      try {
        const saved = await apiJson("/api/saved-queries", {
          method: "POST",
          body: JSON.stringify({
            connection_id: state.workspace.connection.id,
            title,
            statement: stmt,
          }),
        });
        state.savedQueries.unshift(saved);
        renderSavedQueries();
        showBanner(`Query disimpan sebagai "${saved.title}".`, "success");
      } catch (err) {
        showBanner(`Gagal menyimpan query: ${err.message}`, "error");
      }
    });

    // Visual Query Builder
    const builderBtn = document.getElementById("btn-toggle-builder");
    const builderDrawer = document.getElementById("visual-builder-drawer");
    builderBtn?.addEventListener("click", () => {
      if (!builderDrawer) return;
      builderDrawer.hidden = !builderDrawer.hidden;
      builderBtn.setAttribute("aria-expanded", builderDrawer.hidden ? "false" : "true");
    });

    document.getElementById("btn-apply-builder")?.addEventListener("click", () => {
      const tbl = document.getElementById("qb-table")?.value;
      const joinTbl = document.getElementById("qb-join-table")?.value;
      const whereTxt = document.getElementById("qb-where")?.value.trim();
      const orderTxt = document.getElementById("qb-order")?.value.trim();
      if (!tbl) return;

      let sql = `SELECT t1.*\nFROM "${tbl}" t1`;
      if (joinTbl) {
        sql += `\nLEFT JOIN "${joinTbl}" t2 ON 1 = 1`;
      }
      if (whereTxt) sql += `\nWHERE ${whereTxt}`;
      if (orderTxt) sql += `\nORDER BY ${orderTxt}`;
      sql += "\nLIMIT 100;";

      const ed = document.getElementById("sql-editor");
      if (ed) {
        ed.value = wasmFormatSql(sql);
        ed.dataset.userEdited = "1";
      }
      runQuery(sql);
    });

    // Data Grid Filters & Staged Operations
    document.getElementById("btn-apply-filter")?.addEventListener("click", () => {
      state.filterText = document.getElementById("quick-filter-input")?.value || "";
      loadWorkspaceItem(state.workspace?.active_item || "");
    });
    document.getElementById("quick-filter-input")?.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") {
        ev.preventDefault();
        state.filterText = ev.target.value;
        loadWorkspaceItem(state.workspace?.active_item || "");
      }
    });

    document.getElementById("grid-insert-table").onclick = () => openModal("ddl-modal");
    document.getElementById("grid-insert-column").onclick = () => {
      switchTab("structure");
      document.getElementById("addcol-name").focus();
    };
    document.getElementById("grid-copy-table").onclick = () => {
      const ws = state.workspace;
      state.copiedTable = {connection: ws.connection.id, connectionName: ws.connection.name, name: ws.active_item};
      renderDataGrid();
      showBanner(`Sumber salinan: ${ws.active_item}. Pilih Tempel tabel untuk membuat tabel baru.`, "info");
    };
    document.getElementById("grid-paste-table").onclick = async () => {
      const source = state.copiedTable;
      if (state.pasting || !source || state.workspace.connection.id !== source.connection) return;
      if (state.insertedRows.length || state.stagedMutations.length) {
        showBanner("Simpan atau batalkan perubahan grid sebelum menempel tabel.", "error");
        return;
      }
      const name = prompt(`Salin seluruh data dan primary key dari ${source.name}. Nama tabel baru:`, `${source.name}_salinan`)?.trim();
      if (!name) return;
      state.pasting = true;
      renderDataGrid();
      try {
        await apiJson(`/api/connections/${encodeURIComponent(source.connection)}/ddl`, {method:"POST", body:JSON.stringify({action:"copy_table",source_table:source.name,table_name:name})});
        if (state.workspace.connection.id === source.connection) await loadWorkspaceItem(name);
        showBanner(`Tabel ${name} berhasil dibuat dari ${source.name}.`, "success");
      } catch (err) { showBanner(err.message, "error"); }
      finally { state.pasting = false; renderDataGrid(); }
    };
    document.getElementById("btn-add-row")?.addEventListener("click", () => {
      if (!state.workspace?.data || !state.workspace?.active_item) {
        showBanner("Buka tabel atau koleksi terlebih dahulu untuk menambah baris.", "error");
        return;
      }
      state.insertedRows.push({});
      rebuildStagedMutations();
      renderDataGrid();
    });

    document.getElementById("btn-discard-staged")?.addEventListener("click", () => {
      clearStagedChanges();
      renderDataGrid();
      showBanner("Semua perubahan tertunda dibatalkan.", "info");
    });

    document.getElementById("btn-preview-staged")?.addEventListener("click", () => {
      const pre = document.getElementById("staged-preview-code");
      if (!pre || !state.workspace) return;
      const stmts = wasmPreviewMutations(state.workspace.connection.engine, state.stagedMutations);
      pre.textContent = stmts.length ? stmts.join("\n") : "-- Tidak ada perubahan yang perlu dieksekusi.";
      openModal("preview-modal");
    });

    document.getElementById("btn-commit-staged")?.addEventListener("click", commitStaged);
    document.getElementById("btn-modal-commit")?.addEventListener("click", commitStaged);

    // Export
    ["csv", "json", "sql"].forEach((fmt) => {
      document.getElementById(`btn-export-${fmt}`)?.addEventListener("click", () => {
        if (!state.workspace?.active_item) return;
        const url = `/api/connections/${encodeURIComponent(state.workspace.connection.id)}/export?item=${encodeURIComponent(state.workspace.active_item)}&format=${fmt}`;
        window.open(url, "_blank", "noopener");
      });
    });

    const toolsButton = document.getElementById("toggle-tools");
    toolsButton.onclick = () => {
      const open = toolsButton.getAttribute("aria-expanded") !== "true";
      toolsButton.setAttribute("aria-expanded", String(open));
      document.getElementById("studio-actions").classList.toggle("tools-open", open);
    };
    document.getElementById("rename-table").onclick = () => changeSchema("rename_table");
    document.getElementById("grid-prev").onclick = () => loadWorkspaceItem(state.workspace.active_item, Math.max(0, state.offset - state.limit));
    document.getElementById("grid-next").onclick = () => loadWorkspaceItem(state.workspace.active_item, state.offset + state.limit);
    document.getElementById("grid-limit").onchange = ev => loadWorkspaceItem(state.workspace.active_item, 0, Number(ev.target.value));
    document.getElementById("toggle-editor").onclick = ev => {
      const pane = document.getElementById("query-pane");
      pane.hidden = !pane.hidden;
      ev.currentTarget.textContent = pane.hidden ? "Tampilkan editor" : "Sembunyikan editor";
      ev.currentTarget.setAttribute("aria-expanded", String(!pane.hidden));
    };
    // Structure & DDL
    document.getElementById("form-add-column")?.addEventListener("submit", async (ev) => {
      ev.preventDefault();
      if (!state.workspace?.active_item) return;
      const name = document.getElementById("addcol-name")?.value.trim();
      const dataType = document.getElementById("addcol-type")?.value;
      const dflt = document.getElementById("addcol-default")?.value.trim();
      const nullable = document.getElementById("addcol-nullable")?.checked ?? true;
      try {
        const rep = await apiJson(`/api/connections/${encodeURIComponent(state.workspace.connection.id)}/ddl`, {
          method: "POST",
          body: JSON.stringify({
            action: "add_column",
            table_name: state.workspace.active_item,
            column: {
              name,
              data_type: dataType,
              nullable,
              is_pk: false,
              default_value: dflt || null,
            },
          }),
        });
        ev.target.reset();
        await loadWorkspaceItem(state.workspace.active_item);
        showBanner(`Kolom ditambahkan: ${rep.executed_sql}`, "success");
      } catch (err) {
        showBanner(`Gagal menambah kolom: ${err.message}`, "error");
      }
    });

    document.getElementById("form-add-index")?.addEventListener("submit", async (ev) => {
      ev.preventDefault();
      if (!state.workspace?.active_item) return;
      const idxName = document.getElementById("addidx-name")?.value.trim();
      const colName = document.getElementById("addidx-col")?.value.trim();
      const unique = document.getElementById("addidx-unique")?.checked ?? false;
      try {
        const rep = await apiJson(`/api/connections/${encodeURIComponent(state.workspace.connection.id)}/ddl`, {
          method: "POST",
          body: JSON.stringify({
            action: "create_index",
            table_name: state.workspace.active_item,
            index_name: idxName,
            columns: colName.split(",").map((s) => s.trim()).filter(Boolean),
            unique,
          }),
        });
        ev.target.reset();
        await loadWorkspaceItem(state.workspace.active_item);
        showBanner(`Indeks dibuat: ${rep.executed_sql}`, "success");
      } catch (err) {
        showBanner(`Gagal membuat indeks: ${err.message}`, "error");
      }
    });

    document.getElementById("btn-truncate-item")?.addEventListener("click", async () => {
      if (!state.workspace?.active_item) return;
      if (!confirm(`Kosongkan seluruh isi ${state.workspace.active_item}?`)) return;
      try {
        const rep = await apiJson(`/api/connections/${encodeURIComponent(state.workspace.connection.id)}/ddl`, {
          method: "POST",
          body: JSON.stringify({ action: "truncate_item", item_name: state.workspace.active_item }),
        });
        await loadWorkspaceItem(state.workspace.active_item);
        showBanner(rep.message, "success");
      } catch (err) {
        showBanner(err.message, "error");
      }
    });

    document.getElementById("btn-drop-item")?.addEventListener("click", async () => {
      if (!state.workspace?.active_item) return;
      if (!confirm(`Hapus objek ${state.workspace.active_item}?`)) return;
      try {
        const rep = await apiJson(`/api/connections/${encodeURIComponent(state.workspace.connection.id)}/ddl`, {
          method: "POST",
          body: JSON.stringify({ action: "drop_item", item_name: state.workspace.active_item }),
        });
        await loadWorkspaceItem("");
        showBanner(rep.message, "success");
      } catch (err) {
        showBanner(err.message, "error");
      }
    });

    // Schema Sync & History
    document.getElementById("btn-compare-schemas")?.addEventListener("click", compareSchemas);
    document.getElementById("btn-apply-sync")?.addEventListener("click", applySchemaSync);
    document.getElementById("btn-refresh-history")?.addEventListener("click", refreshHistory);

    // Modals
    document.getElementById("btn-discover-server")?.addEventListener("click", () => {
      openModal("discovery-modal");
      runServerDiscovery();
    });
    document.getElementById("btn-rescan-server")?.addEventListener("click", runServerDiscovery);
    document.getElementById("btn-new-connection")?.addEventListener("click", () => openModal("conn-modal"));
    document.getElementById("btn-new-table")?.addEventListener("click", () => openModal("ddl-modal"));
    document.getElementById("btn-command-palette")?.addEventListener("click", () => {
      openModal("palette-modal");
      renderPalette("");
    });

    document.querySelectorAll(".modal-close").forEach((btn) => {
      btn.addEventListener("click", () => closeModal(btn.dataset.close));
    });
    document.querySelectorAll(".modal-backdrop").forEach((bd) => {
      bd.addEventListener("click", (ev) => {
        if (ev.target === bd) closeModal(bd.id);
      });
    });

    document.getElementById("btn-test-conn")?.addEventListener("click", async () => {
      const engine = document.getElementById("conn-engine")?.value;
      const target = document.getElementById("conn-target")?.value.trim();
      const box = document.getElementById("conn-test-result");
      if (!box) return;
      box.hidden = false;
      box.textContent = "Menguji koneksi...";
      try {
        const res = await apiJson("/api/connections/test", {
          method: "POST",
          body: JSON.stringify({ engine, target }),
        });
        box.textContent = `Terhubung (${res.latency_ms} ms): ${res.server_version} | ${res.details}`;
      } catch (err) {
        box.textContent = `Gagal terhubung: ${err.message}`;
      }
    });

    document.getElementById("form-new-connection")?.addEventListener("submit", async (ev) => {
      ev.preventDefault();
      const name = document.getElementById("conn-name")?.value.trim();
      const engine = document.getElementById("conn-engine")?.value;
      const environment = document.getElementById("conn-env")?.value;
      const target = document.getElementById("conn-target")?.value.trim();
      try {
        const created = await apiJson("/api/connections", {
          method: "POST",
          body: JSON.stringify({ name, engine, environment, target }),
        });
        state.connections.push(created);
        closeModal("conn-modal");
        ev.target.reset();
        await switchConnection(created.id);
        showBanner(`Koneksi '${created.name}' tersimpan dan aktif.`, "success");
      } catch (err) {
        const box = document.getElementById("conn-test-result");
        if (box) {
          box.hidden = false;
          box.textContent = err.message;
        }
      }
    });

    document.getElementById("form-new-table")?.addEventListener("submit", async (ev) => {
      ev.preventDefault();
      if (!state.workspace) return;
      const tableName = document.getElementById("new-table-name")?.value.trim();
      const rawCols = document.getElementById("new-table-cols")?.value || "";
      const columns = rawCols
        .split("\n")
        .map((line) => line.trim())
        .filter(Boolean)
        .map((line) => {
          const [name, dataType = "TEXT", flag = "NN"] = line.split(":").map((s) => s.trim());
          return {
            name,
            data_type: dataType,
            is_pk: flag.toUpperCase() === "PK",
            nullable: flag.toUpperCase() === "NULL",
            default_value: null,
          };
        });
      try {
        const rep = await apiJson(`/api/connections/${encodeURIComponent(state.workspace.connection.id)}/ddl`, {
          method: "POST",
          body: JSON.stringify({ action: "create_table", table_name: tableName, columns }),
        });
        closeModal("ddl-modal");
        await loadWorkspaceItem(tableName);
        showBanner(`Objek '${tableName}' dibuat: ${rep.executed_sql}`, "success");
      } catch (err) {
        showBanner(`Gagal membuat objek: ${err.message}`, "error");
      }
    });

    document.getElementById("palette-input")?.addEventListener("input", (ev) => {
      renderPalette(ev.target.value);
    });

    // Global keyboard shortcuts
    document.addEventListener("keydown", (ev) => {
      const modal = document.querySelector(".modal-backdrop:not([hidden])");
      if (modal) {
        if (ev.key === "Tab") {
          const controls = [...modal.querySelectorAll("button, input, select, textarea, [href]")].filter(el => !el.disabled && el.getClientRects().length);
          const end = ev.shiftKey ? controls[0] : controls.at(-1);
          if (end && document.activeElement === end) {
            ev.preventDefault();
            (ev.shiftKey ? controls.at(-1) : controls[0]).focus();
          }
        } else if (ev.key === "Escape") closeModal(modal.id);
        return;
      }
      if (ev.key === "Escape") {
        closeAllModals();
      } else if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "k") {
        ev.preventDefault();
        openModal("palette-modal");
        renderPalette("");
      } else if ((ev.ctrlKey || ev.metaKey) && ev.key === "Enter") {
        ev.preventDefault();
        runQuery();
      } else if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "s") {
        if (state.stagedMutations.length > 0) {
          ev.preventDefault();
          commitStaged();
        }
      }
    });
  }

  function renderPalette(query) {
    const ul = document.getElementById("palette-results");
    if (!ul) return;
    const q = query.trim().toLowerCase();
    ul.innerHTML = "";
    const entries = [];
    for (const c of state.connections) {
      entries.push({ label: `Koneksi: ${c.name}`, meta: c.engine, action: () => switchConnection(c.id) });
    }
    for (const item of state.workspace?.items || []) {
      entries.push({ label: `Objek: ${item.name}`, meta: `${item.row_count} ${item.item_type}`, action: () => loadWorkspaceItem(item.name) });
    }
    for (const e of entries.filter((x) => x.label.toLowerCase().includes(q))) {
      const li = document.createElement("li");
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "tree-btn";
      btn.innerHTML = `<span class="tree-label">${escapeHtml(e.label)}</span><span class="tree-meta">${escapeHtml(e.meta)}</span>`;
      btn.addEventListener("click", () => {
        closeModal("palette-modal");
        e.action();
      });
      li.appendChild(btn);
      ul.appendChild(li);
    }
  }

  async function runServerDiscovery() {
    const sumEl = document.getElementById("discovery-summary");
    const resEl = document.getElementById("discovery-results");
    if (!sumEl || !resEl) return;
    sumEl.innerHTML = '<span class="meta-text">Memindai engine, port, konfigurasi, dan database aktif di server ini...</span>';
    resEl.innerHTML = "";
    try {
      const report = await apiJson("/api/discovery/scan");
      renderDiscoveryReport(report);
    } catch (err) {
      sumEl.innerHTML = `<div class="status-banner is-error">Gagal memindai server: ${escapeHtml(err.message)}</div>`;
    }
  }

  function renderDiscoveryReport(report) {
    const sumEl = document.getElementById("discovery-summary");
    const resEl = document.getElementById("discovery-results");
    if (!sumEl || !resEl) return;

    sumEl.innerHTML = `
      <span><strong>Host:</strong> ${escapeHtml(report.hostname)}</span>
      <span><strong>OS:</strong> ${escapeHtml(report.os_info)}</span>
      <span><strong>Engine Terdeteksi:</strong> ${report.total_engines_detected} / ${report.engines.length} (${report.running_engines_count} aktif)</span>
      <span><strong>Database Ditemukan:</strong> ${report.total_databases_found}</span>
    `;

    resEl.innerHTML = "";
    for (const eng of report.engines) {
      const card = document.createElement("section");
      card.className = "discovery-card";

      const badgeClass =
        eng.status === "running"
          ? "env-local"
          : eng.status === "installed_stopped"
          ? "env-staging"
          : "env-prod";
      const statusText =
        eng.status === "running"
          ? "AKTIF"
          : eng.status === "installed_stopped"
          ? "TERPASANG (MATI)"
          : "TIDAK TERPASANG";

      const head = document.createElement("div");
      head.className = "discovery-card-head";
      const titleRow = document.createElement("div");
      titleRow.className = "discovery-title-row";
      titleRow.innerHTML = `
        <strong>${escapeHtml(eng.engine_label)}</strong>
        <span class="env-pill ${badgeClass}">${statusText}</span>
        <span class="meta-text">${escapeHtml(eng.version)}</span>
      `;
      head.appendChild(titleRow);

      const actionWrap = document.createElement("div");
      actionWrap.className = "discovery-head-actions";
      const needsAuth = Object.keys(eng.parameters || {}).some((k) => k.startsWith("auth."));
      if (eng.status === "running" && needsAuth) {
        const credBtn = document.createElement("button");
        credBtn.type = "button";
        credBtn.className = "btn btn-secondary btn-sm";
        credBtn.textContent = "+ Koneksi Kredensial";
        credBtn.addEventListener("click", () => {
          closeModal("discovery-modal");
          const engSel = document.getElementById("conn-engine");
          const targetInp = document.getElementById("conn-target");
          const nameInp = document.getElementById("conn-name");
          if (engSel) engSel.value = eng.engine;
          if (nameInp) nameInp.value = `${eng.engine_label} (${eng.endpoint || "local"})`;
          if (targetInp) {
            const ep = eng.endpoint || (eng.engine === "postgres" ? "127.0.0.1:5432" : "127.0.0.1:3306");
            targetInp.value =
              eng.engine === "postgres"
                ? `postgres://postgres:password@${ep}/postgres`
                : `mysql://root:password@${ep}/mysql`;
          }
          openModal("conn-modal");
        });
        actionWrap.appendChild(credBtn);
      }

      if (eng.can_provision_local) {
        const provBtn = document.createElement("button");
        provBtn.type = "button";
        provBtn.className = "btn btn-primary btn-sm";
        provBtn.textContent = `Nyalakan & Hubungkan ${eng.engine_label} Lokal`;
        provBtn.addEventListener("click", async () => {
          provBtn.disabled = true;
          provBtn.textContent = "Menyalakan instance...";
          try {
            const out = await apiJson("/api/discovery/provision", {
              method: "POST",
              body: JSON.stringify({ engine: eng.engine }),
            });
            const exists = state.connections.some((c) => c.id === out.connection.id);
            if (!exists) state.connections.push(out.connection);
            renderDiscoveryReport(out.discovery);
            await switchConnection(out.connection.id);
            showBanner(out.message, "success");
          } catch (err) {
            provBtn.disabled = false;
            provBtn.textContent = `Nyalakan & Hubungkan ${eng.engine_label} Lokal`;
            sumEl.innerHTML = `<div class="status-banner is-error">${escapeHtml(err.message)}</div>`;
          }
        });
        actionWrap.appendChild(provBtn);
      }
      if (actionWrap.children.length > 0) head.appendChild(actionWrap);
      card.appendChild(head);

      const metaLine = document.createElement("div");
      metaLine.className = "meta-text";
      metaLine.textContent = `Binary: ${eng.binary_path || "-"} | Endpoint: ${eng.endpoint || "-"}${eng.latency_ms !== null && eng.latency_ms !== undefined ? ` | Latensi: ${eng.latency_ms} ms` : ""}`;
      card.appendChild(metaLine);

      const paramEntries = Object.entries(eng.parameters || {});
      if (paramEntries.length > 0) {
        const pGrid = document.createElement("div");
        pGrid.className = "discovery-params";
        for (const [k, v] of paramEntries) {
          const row = document.createElement("div");
          row.className = "discovery-param-row";
          row.innerHTML = `<code>${escapeHtml(k)}</code><span>${escapeHtml(v)}</span>`;
          pGrid.appendChild(row);
        }
        card.appendChild(pGrid);
      }

      if (eng.databases && eng.databases.length > 0) {
        const dbList = document.createElement("div");
        dbList.className = "discovery-db-list";
        for (const db of eng.databases) {
          const row = document.createElement("div");
          row.className = "discovery-db-row";
          row.innerHTML = `
            <div>
              <strong>${escapeHtml(db.name)}</strong>
              <span class="meta-text"> (${escapeHtml(db.size_label)}, ${db.object_count} objek) | <code>${escapeHtml(db.connection_target)}</code></span>
            </div>
          `;
          if (eng.status === "running" || db.already_registered) {
            const actionBtn = document.createElement("button");
            actionBtn.type = "button";
            actionBtn.className = db.already_registered ? "btn btn-secondary btn-sm" : "btn btn-primary btn-sm";
            actionBtn.textContent = db.already_registered ? "Buka di Studio" : "+ Hubungkan";
            actionBtn.addEventListener("click", async () => {
              if (db.already_registered && db.existing_connection_id) {
                closeModal("discovery-modal");
                await switchConnection(db.existing_connection_id);
                return;
              }
              try {
                const created = await apiJson("/api/connections", {
                  method: "POST",
                  body: JSON.stringify({
                    name: `${eng.engine_label}: ${db.name}`,
                    engine: db.engine,
                    environment: "local",
                    target: db.connection_target,
                  }),
                });
                state.connections.push(created);
                closeModal("discovery-modal");
                await switchConnection(created.id);
                showBanner(`Database '${db.name}' terhubung ke Studio.`, "success");
              } catch (err) {
                showBanner(`Gagal menghubungkan database: ${err.message}`, "error");
              }
            });
            row.appendChild(actionBtn);
          } else {
            const note = document.createElement("span");
            note.className = "meta-text";
            note.textContent = "Daemon tidak aktif";
            row.appendChild(note);
          }
          dbList.appendChild(row);
        }
        card.appendChild(dbList);
      }

      resEl.appendChild(card);
    }
  }

  async function loadBackupsDashboard() {
    const dirLbl = document.getElementById("backup-dir-label");
    const schedBody = document.getElementById("schedule-list-tbody");
    const bkpBody = document.getElementById("backup-list-tbody");
    if (!schedBody || !bkpBody) return;
    try {
      const data = await apiJson("/api/backups");
      if (dirLbl) dirLbl.textContent = data.backup_directory;

      schedBody.innerHTML = "";
      if (!data.schedules.length) {
        schedBody.innerHTML = '<tr><td colspan="6" class="meta-text">Belum ada jadwal backup otomatis.</td></tr>';
      } else {
        for (const s of data.schedules) {
          const tr = document.createElement("tr");
          tr.innerHTML = `
            <td><strong>${escapeHtml(s.connection_name)}</strong></td>
            <td>Setiap ${s.interval_minutes} menit</td>
            <td>Max ${s.retention_count} file</td>
            <td><span class="env-pill ${s.enabled ? "env-local" : "env-staging"}">${s.enabled ? "AKTIF" : "JEDA"}</span></td>
            <td><code>${escapeHtml(s.next_run_at.replace("T", " ").slice(0, 19))}</code></td>
            <td><div class="action-btn-cluster"></div></td>
          `;
          const act = tr.querySelector(".action-btn-cluster");
          const runBtn = document.createElement("button");
          runBtn.type = "button";
          runBtn.className = "btn btn-primary btn-sm";
          runBtn.textContent = "Jalankan Sekarang";
          runBtn.addEventListener("click", async () => {
            try {
              const rec = await apiJson(`/api/backups/schedules/${encodeURIComponent(s.id)}/run`, { method: "POST" });
              await loadBackupsDashboard();
              await refreshHistory();
              showBanner(`Backup terjadwal dijalankan: ${rec.file_name}`, "success");
            } catch (err) {
              showBanner(err.message, "error");
            }
          });
          act.appendChild(runBtn);

          const togBtn = document.createElement("button");
          togBtn.type = "button";
          togBtn.className = "btn btn-secondary btn-sm";
          togBtn.textContent = s.enabled ? "Jeda" : "Aktifkan";
          togBtn.addEventListener("click", async () => {
            await apiJson(`/api/backups/schedules/${encodeURIComponent(s.id)}/toggle`, {
              method: "POST",
              body: JSON.stringify({ enabled: !s.enabled }),
            });
            loadBackupsDashboard();
          });
          act.appendChild(togBtn);

          const delBtn = document.createElement("button");
          delBtn.type = "button";
          delBtn.className = "btn btn-danger-outline btn-sm";
          delBtn.textContent = "Hapus Jadwal";
          delBtn.addEventListener("click", async () => {
            await apiJson(`/api/backups/schedules/${encodeURIComponent(s.id)}`, { method: "DELETE" });
            loadBackupsDashboard();
          });
          act.appendChild(delBtn);

          schedBody.appendChild(tr);
        }
      }

      bkpBody.innerHTML = "";
      if (!data.backups.length) {
        bkpBody.innerHTML = '<tr><td colspan="7" class="meta-text">Belum ada arsip backup. Gunakan form Backup Manual atau Jalankan Jadwal.</td></tr>';
      } else {
        for (const b of data.backups) {
          const tr = document.createElement("tr");
          tr.innerHTML = `
            <td>${escapeHtml(b.created_at.replace("T", " ").slice(0, 19))}</td>
            <td><strong>${escapeHtml(b.connection_name)}</strong></td>
            <td><span class="env-pill env-local">${escapeHtml(b.kind)}</span></td>
            <td><div>${escapeHtml(b.label)}</div><code>${escapeHtml(b.file_name)}</code></td>
            <td>${b.file_size_bytes} B / ${b.row_count} baris</td>
            <td><code>${escapeHtml(b.sha256.slice(0, 12))}...</code></td>
            <td><div class="action-btn-cluster"></div></td>
          `;
          const act = tr.querySelector(".action-btn-cluster");

          const verBtn = document.createElement("button");
          verBtn.type = "button";
          verBtn.className = "btn btn-secondary btn-sm";
          verBtn.textContent = "Verifikasi";
          verBtn.addEventListener("click", async () => {
            try {
              const v = await apiJson(`/api/backups/${encodeURIComponent(b.id)}/verify`, { method: "POST" });
              showBanner(v.message, v.valid ? "success" : "error");
            } catch (err) {
              showBanner(err.message, "error");
            }
          });
          act.appendChild(verBtn);

          const dlBtn = document.createElement("button");
          dlBtn.type = "button";
          dlBtn.className = "btn btn-secondary btn-sm";
          dlBtn.textContent = "Unduh";
          dlBtn.addEventListener("click", () => {
            window.open(`/api/backups/${encodeURIComponent(b.id)}/download`, "_blank", "noopener");
          });
          act.appendChild(dlBtn);

          const rstBtn = document.createElement("button");
          rstBtn.type = "button";
          rstBtn.className = "btn btn-primary btn-sm";
          rstBtn.textContent = "Pulihkan ke DB Baru";
          rstBtn.addEventListener("click", async () => {
            try {
              const out = await apiJson(`/api/backups/${encodeURIComponent(b.id)}/restore`, {
                method: "POST",
                body: JSON.stringify({ new_connection_name: null }),
              });
              state.connections.push(out.connection);
              await switchConnection(out.connection.id);
              showBanner(out.message, "success");
            } catch (err) {
              showBanner(`Restore gagal: ${err.message}`, "error");
            }
          });
          act.appendChild(rstBtn);

          bkpBody.appendChild(tr);
        }
      }
    } catch (err) {
      showBanner(`Gagal memuat daftar backup: ${err.message}`, "error");
    }
  }

  // --- Boot ---
  function initTheme() {
    let saved = null;
    try {
      saved = localStorage.getItem("guidb_theme");
    } catch {
      saved = null;
    }
    const prefersDark = window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
    document.documentElement.setAttribute("data-theme", saved || (prefersDark ? "dark" : "light"));
  }

  function boot() {
    initTheme();
    const rawTag = document.getElementById("guidb-bootstrap-data");
    if (rawTag && rawTag.textContent) {
      try {
        const bootData = JSON.parse(rawTag.textContent);
        state.connections = bootData.connections || [];
        state.workspace = bootData.workspace || null;
        state.history = bootData.history || [];
        state.savedQueries = bootData.saved_queries || [];
      } catch (err) {
        showBanner(`Gagal membaca data awal SSR: ${err.message}`, "error");
      }
    }
    bindEvents();
    if (window.matchMedia("(max-width: 600px)").matches) document.getElementById("toggle-editor").click();
    renderConnections();
    renderWorkspace();
    renderHistory();
    initWasm();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot);
  } else {
    boot();
  }
})();
