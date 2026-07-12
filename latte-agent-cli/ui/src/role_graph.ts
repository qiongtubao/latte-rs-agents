// Role × Tool graph panel — fetches /api/role-graph and renders the
// role/tool/registration nodes with their edges. The actual computation
// is performed on the Rust side via the latte-rs-graph library
// (TreeSitterEngine over the project source). This file is the wire-shape
// adapter and DOM renderer.

interface RoleGraphNode {
  id: string;
  kind: "Role" | "Tool" | "ToolRegistration";
  label: string;
  detail?: string;
}
interface RoleGraphEdge {
  source: string;
  target: string;
  kind: "USES_TOOL" | "REGISTERED_BY";
}
interface RoleGraph {
  nodes: RoleGraphNode[];
  edges: RoleGraphEdge[];
  stats: { roles: number; tools: number; registrations: number };
  project_root: string;
}

async function fetchRoleGraph(): Promise<RoleGraph> {
  const r = await fetch("/api/role-graph");
  if (!r.ok) throw new Error(`GET /api/role-graph ${r.status}`);
  return r.json();
}

interface UIBinding {
  panelEl: HTMLElement;
  openBtn: HTMLButtonElement;
  closeBtn: HTMLButtonElement;
  refreshBtn: HTMLButtonElement;
  statsEl: HTMLElement;
  bodyEl: HTMLElement;
}

export interface RoleGraphController {
  isOpen(): boolean;
  open(): void;
}

export function mountRoleGraph(opts: {
  container: UIBinding;
}): RoleGraphController {
  const { container } = opts;
  let isLoading = false;

  container.openBtn.addEventListener("click", () => {
    container.panelEl.classList.remove("hidden");
    void refresh();
  });
  container.closeBtn.addEventListener("click", () => {
    container.panelEl.classList.add("hidden");
  });
  container.refreshBtn.addEventListener("click", () => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    if (isLoading) return;
    isLoading = true;
    container.statsEl.textContent = "loading…";
    container.bodyEl.innerHTML = "";
    try {
      const g = await fetchRoleGraph();
      render(g, container);
    } catch (e) {
      container.statsEl.textContent = `error: ${(e as Error).message}`;
    } finally {
      isLoading = false;
    }
  }

  return {
    isOpen: () => !container.panelEl.classList.contains("hidden"),
    open: () => container.panelEl.classList.remove("hidden"),
  };
}

function render(g: RoleGraph, c: UIBinding): void {
  c.statsEl.textContent = JSON.stringify(g.stats);
  c.bodyEl.innerHTML = "";

  // Roles grouped with their USES_TOOL destinations.
  const toolsByRole = new Map<string, RoleGraphNode[]>();
  const registrations: RoleGraphNode[] = [];
  const standaloneTools = new Set<string>();
  for (const e of g.edges) {
    if (e.kind === "USES_TOOL") {
      const tgt = g.nodes.find((n) => n.id === e.target);
      if (!tgt) continue;
      const list = toolsByRole.get(e.source) ?? [];
      list.push(tgt);
      toolsByRole.set(e.source, list);
      standaloneTools.delete(tgt.id);
    } else if (e.kind === "REGISTERED_BY") {
      const src = g.nodes.find((n) => n.id === e.source);
      if (src) registrations.push(src);
    }
  }
  for (const n of g.nodes) {
    if (n.kind === "Tool") standaloneTools.add(n.id);
  }

  const wrap = document.createElement("div");
  wrap.className = "role-graph-content";

  // Roles section.
  for (const roleNode of g.nodes.filter((n) => n.kind === "Role")) {
    const card = document.createElement("div");
    card.className = "role-graph-card role";
    card.dataset.roleId = roleNode.id;
    card.dataset.kind = "Role";

    const title = document.createElement("div");
    title.className = "role-graph-card-title";
    title.textContent = roleNode.label;

    const sub = document.createElement("div");
    sub.className = "role-graph-card-sub";
    sub.textContent = roleNode.detail ?? "";

    const tools = document.createElement("ul");
    tools.className = "role-graph-tools";
    for (const t of toolsByRole.get(roleNode.id) ?? []) {
      const li = document.createElement("li");
      li.className = "role-graph-tool";
      li.textContent = `· ${t.label}`;
      li.dataset.toolId = t.id;
      tools.appendChild(li);
    }

    card.appendChild(title);
    card.appendChild(sub);
    card.appendChild(tools);
    wrap.appendChild(card);
  }

  // Tool registrations section.
  if (registrations.length) {
    const heading = document.createElement("div");
    heading.className = "role-graph-section-heading";
    heading.textContent = `ToolRegistration (${registrations.length})`;
    wrap.appendChild(heading);
    for (const r of registrations) {
      const card = document.createElement("div");
      card.className = "role-graph-card registration";
      card.dataset.registrationId = r.id;
      const title = document.createElement("div");
      title.className = "role-graph-card-title";
      title.textContent = r.label;
      card.appendChild(title);
      const sub = document.createElement("div");
      sub.className = "role-graph-card-sub";
      sub.textContent = r.detail ?? "";
      card.appendChild(sub);
      wrap.appendChild(card);
    }
  }

  c.bodyEl.appendChild(wrap);
}
