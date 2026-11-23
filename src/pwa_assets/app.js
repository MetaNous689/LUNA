"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  const bytes = (value) => {
    if (!Number.isFinite(value) || value < 0) return "—";
    const units = ["B", "KB", "MB", "GB", "TB"];
    let size = value;
    let unit = 0;
    while (size >= 1024 && unit < units.length - 1) {
      size /= 1024;
      unit += 1;
    }
    const digits = size >= 100 || unit === 0 ? 0 : size >= 10 ? 1 : 2;
    return `${size.toLocaleString("de-DE", { maximumFractionDigits: digits, minimumFractionDigits: digits })} ${units[unit]}`;
  };
  const percent = (value, digits = 0) =>
    Number.isFinite(value) ? `${value.toLocaleString("de-DE", { maximumFractionDigits: digits, minimumFractionDigits: digits })} %` : "—";
  const clamp = (value, low, high) => Math.min(high, Math.max(low, value));

  function setText(id, value) {
    const node = $(id);
    if (node) node.textContent = value == null || value === "" ? "—" : String(value);
  }

  function setPath(id, values, maximum = null) {
    const path = $(id);
    if (!path) return;
    const valid = Array.isArray(values) ? values.filter(Number.isFinite) : [];
    if (valid.length < 2) {
      path.setAttribute("d", "");
      return;
    }
    const max = maximum ?? Math.max(1, ...valid);
    const last = valid.length - 1;
    const points = valid.map((value, index) => {
      const x = (index / last) * 100;
      const y = 22 - clamp(value / max, 0, 1) * 19;
      return `${index === 0 ? "M" : "L"}${x.toFixed(1)} ${y.toFixed(1)}`;
    });
    path.setAttribute("d", points.join(" "));
  }

  function renderAgents(payload) {
    const list = $("agent-list");
    list.replaceChildren();
    const agents = (Array.isArray(payload.items) ? payload.items : [])
      .map((agent) => ({ agent, state: safeState(agent.state) }));
    $("agents-note").textContent = payload.status === "ready" ? `${agents.length}` : statusText(payload.status);
    if (payload.status === "loading" || payload.status === "unavailable") {
      list.appendChild(emptyItem(payload.status === "loading" ? "Agentenstatus wird geladen" : "Keine Agenten-Quelle erreichbar"));
      return;
    }
    if (agents.length === 0) {
      const message = payload.status === "stale"
        ? "Keine aktuellen Agentendaten"
        : "Keine bestätigten Agentenstatus";
      list.appendChild(emptyItem(message));
      return;
    }
    for (const { agent, state: agentState } of agents) {
      const item = document.createElement("li");
      item.className = "status-row";
      const name = document.createElement("span");
      name.className = "agent-name";
      const label = document.createElement("span");
      label.textContent = agent.name || "agent";
      if (agentState !== null) {
        const dot = document.createElement("span");
        dot.className = "agent-dot";
        dot.dataset.state = agentState;
        dot.setAttribute("aria-hidden", "true");
        name.appendChild(dot);
      }
      name.appendChild(label);
      item.appendChild(name);
      if (agentState !== null) {
        const state = document.createElement("span");
        state.className = "agent-state";
        state.textContent = agentState;
        item.appendChild(state);
      }
      list.appendChild(item);
    }
  }

  function renderServers(payload) {
    const list = $("server-list");
    list.replaceChildren();
    const servers = Array.isArray(payload.items) ? payload.items : [];
    $("servers-note").textContent = payload.status === "ready" ? `${servers.length}` : statusText(payload.status);
    if (payload.status === "loading" || payload.status === "unavailable") {
      list.appendChild(emptyItem(payload.status === "loading" ? "Serverliste wird geladen" : "Serverliste nicht verfügbar"));
      return;
    }
    if (servers.length === 0) {
      list.appendChild(emptyItem(payload.status === "stale" ? "Letzter Stand · keine Server" : "Keine lokalen Server erkannt"));
      return;
    }
    for (const server of servers) {
      const item = document.createElement("li");
      item.className = "status-row";
      const name = document.createElement("span");
      name.className = "server-name";
      name.textContent = server.name || "Serverdienst";
      const detail = document.createElement("span");
      detail.className = "server-detail";
      const parts = [];
      if (Number.isFinite(server.port)) parts.push(`:${server.port}`);
      if (Number.isFinite(server.pid)) parts.push(`PID ${server.pid}`);
      detail.textContent = parts.join(" · ") || "—";
      item.append(name, detail);
      list.appendChild(item);
    }
  }

  function emptyItem(text) {
    const item = document.createElement("li");
    item.className = "list-empty";
    item.textContent = text;
    return item;
  }

  function safeState(value) {
    return ["working", "blocked", "done", "idle"].includes(value) ? value : null;
  }

  function statusText(value) {
    return ({ loading: "Wird geladen", unavailable: "Nicht verfügbar", stale: "Veraltet", ready: "Aktuell" })[value] || "Nicht verfügbar";
  }

  function render(payload) {
    const cpu = payload.cpu || {};
    const memory = payload.memory || {};
    const gpu = payload.gpu || {};
    const disk = payload.disk || {};
    const network = payload.network || {};
    const cpuLoad = Number.isFinite(cpu.load_percent) ? clamp(cpu.load_percent, 0, 100) : null;
    const cpuFree = cpuLoad == null ? null : 100 - cpuLoad;
    const memoryFree = Number.isFinite(memory.available_bytes) ? memory.available_bytes : null;
    const gpuLoad = Number.isFinite(gpu.load_percent) ? clamp(gpu.load_percent, 0, 100) : null;

    setText("cpu-value", cpuFree == null ? "—" : percent(cpuFree));
    setText("cpu-detail", cpuLoad == null ? "Noch keine Messung" : `${percent(cpuLoad)} ausgelastet`);
    setText("ram-value", bytes(memoryFree));
    setText("ram-detail", Number.isFinite(memory.total_bytes) ? `von ${bytes(memory.total_bytes)}` : "Noch keine Messung");
    const swapKnown = Number.isFinite(memory.swap_used_bytes) && Number.isFinite(memory.swap_total_bytes)
      && memory.swap_used_bytes >= 0 && memory.swap_used_bytes <= memory.swap_total_bytes;
    setText("ram-swap", swapKnown ? `${bytes(memory.swap_used_bytes)} / ${bytes(memory.swap_total_bytes)}` : "—");
    const pressure = (value) => Number.isFinite(value) && value >= 0 && value <= 100 ? percent(value, 2) : "—";
    setText("ram-psi-some", pressure(memory.psi_some_avg10_percent));
    setText("ram-psi-full", pressure(memory.psi_full_avg10_percent));
    setText("gpu-value", percent(gpuLoad));
    setText("gpu-detail", gpuLoad == null ? "Nicht verfügbar" : "Auslastung");
    setText("vram-value", bytes(gpu.vram_free_bytes));
    setText("vram-detail", Number.isFinite(gpu.vram_total_bytes) ? `von ${bytes(gpu.vram_total_bytes)}` : "Nicht verfügbar");
    setText("disk-value", bytes(disk.available_bytes));
    setText("disk-detail", Number.isFinite(disk.total_bytes) ? `von ${bytes(disk.total_bytes)}` : "Noch keine Messung");

    renderQuota("codex", payload.codex);
    renderQuota("claude", payload.claude);

    const download = Number.isFinite(network.download_bytes_per_second) ? network.download_bytes_per_second : null;
    const upload = Number.isFinite(network.upload_bytes_per_second) ? network.upload_bytes_per_second : null;
    setText("cpu-now", percent(cpuLoad));
    setText("ram-now", Number.isFinite(memory.used_bytes) && Number.isFinite(memory.total_bytes) && memory.total_bytes > 0
      ? percent(memory.used_bytes / memory.total_bytes * 100)
      : "—");
    setText("gpu-now", percent(gpuLoad));
    setText("network-now", download == null && upload == null ? "—" : `${bytes(download || 0)}/s`);
    setText("network-detail", download == null && upload == null
      ? "Netzwerkdurchsatz nicht verfügbar"
      : `↓ ${bytes(download || 0)}/s  ·  ↑ ${bytes(upload || 0)}/s`);
    setPath("cpu-spark", cpu.history, 100);
    setPath("ram-spark", payload.memory_history, 100);
    setPath("gpu-spark", gpu.history, 100);
    setPath("cpu-trend", cpu.history, 100);
    setPath("ram-trend", payload.memory_history, 100);
    setPath("gpu-trend", gpu.history, 100);
    setPath("network-trend", combineRates(network.download_history, network.upload_history));
    renderAgents(payload.agents || { status: "unavailable", items: [] });
    renderServers(payload.servers || { status: "unavailable", items: [] });
  }

  function renderQuota(name, line) {
    // A tool that is not installed sends no line and gets no card.
    const card = $(`${name}-meter`).closest(`.metric-${name}`);
    card.hidden = line == null;
    if (line == null) return;
    const parts = String(line || "").replace(new RegExp(`^${name}\\s*`, "i"), "").split(" · ");
    const free = (parts.shift() || "—").trim();
    const remainder = free.match(/^(\d+(?:[.,]\d+)?)\s*%(?:\s+frei)?$/i);
    const remainingPercent = remainder ? clamp(Number(remainder[1].replace(",", ".")), 0, 100) : null;
    setText(`${name}-value`, remainingPercent == null ? free : percent(remainingPercent, 1));
    setText(`${name}-detail`, parts.join(" · ") || "Wochenlimit nicht verfügbar");
    const meter = $(`${name}-meter`);
    meter.hidden = remainingPercent == null;
    if (remainingPercent != null) meter.value = remainingPercent;
    meter.closest(`.metric-${name}`).dataset.availability = remainingPercent == null ? "unavailable" : "ready";
  }

  function combineRates(download, upload) {
    if (!Array.isArray(download) || !Array.isArray(upload)) return [];
    return download.map((value, index) => {
      const other = upload[index];
      if (!Number.isFinite(value) && !Number.isFinite(other)) return null;
      return (Number.isFinite(value) ? value : 0) + (Number.isFinite(other) ? other : 0);
    });
  }

  function setConnection(state, label) {
    const node = $("connection");
    if (node.dataset.state !== state || $("connection-label").textContent !== label) {
      node.dataset.state = state;
      $("connection-label").textContent = label;
    }
    document.body.dataset.connection = state;
  }

  let refreshInFlight = false;
  let lastUpdated = null;

  async function refresh() {
    if (refreshInFlight) return;
    refreshInFlight = true;
    const button = $("refresh");
    button.disabled = true;
    button.setAttribute("aria-busy", "true");
    const controller = new AbortController();
    const timeout = window.setTimeout(() => controller.abort(), 8000);
    try {
      const response = await fetch("/api/state", { cache: "no-store", headers: { Accept: "application/json" }, signal: controller.signal });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const payload = await response.json();
      render(payload);
      setConnection("online", "Live");
      lastUpdated = new Date().toLocaleTimeString("de-DE", { hour: "2-digit", minute: "2-digit", second: "2-digit" });
      setText("updated", `Aktualisiert ${lastUpdated}`);
    } catch {
      setConnection("offline", "Offline");
      setText("updated", lastUpdated ? `Letzter Stand ${lastUpdated}` : "Keine Verbindung zum Monitor");
      if (!lastUpdated) {
        for (const name of ["codex", "claude"]) {
          setText(`${name}-detail`, "Wochenlimit nicht verfügbar");
          $(`${name}-meter`).closest(`.metric-${name}`).dataset.availability = "unavailable";
        }
        for (const [list, note] of [["agent-list", "agents-note"], ["server-list", "servers-note"]]) {
          setText(note, "Offline");
          $(list).replaceChildren(emptyItem("Keine Verbindung zum Monitor"));
        }
      }
    } finally {
      window.clearTimeout(timeout);
      refreshInFlight = false;
      button.disabled = false;
      button.setAttribute("aria-busy", "false");
    }
  }

  $("refresh").addEventListener("click", refresh);
  window.addEventListener("online", refresh);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") refresh();
  });
  refresh();
  window.setInterval(() => {
    if (document.visibilityState === "visible") refresh();
  }, 3000);

  let installPrompt = null;
  const isAppleMobile = () => /iPhone|iPad|iPod/i.test(navigator.userAgent)
    || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
  const isStandalone = () => window.matchMedia("(display-mode: standalone)").matches
    || navigator.standalone === true;

  if (isAppleMobile() && !isStandalone()) {
    $("install").textContent = "Zum Home-Bildschirm";
    $("install").hidden = false;
  }

  window.addEventListener("beforeinstallprompt", (event) => {
    event.preventDefault();
    if (isStandalone()) return;
    installPrompt = event;
    $("install").hidden = false;
  });
  $("install").addEventListener("click", async () => {
    if (isAppleMobile() && !isStandalone()) {
      $("install-help").textContent = "Im Browser-Menü „Teilen“ → „Zum Home-Bildschirm“ wählen.";
      $("install-help").hidden = false;
      return;
    }
    if (!installPrompt) return;
    installPrompt.prompt();
    await installPrompt.userChoice;
    installPrompt = null;
    $("install").hidden = true;
  });

  if ("serviceWorker" in navigator && window.isSecureContext) {
    navigator.serviceWorker.register("/service-worker.js", { scope: "/", updateViaCache: "none" }).catch(() => {});
  }
})();
