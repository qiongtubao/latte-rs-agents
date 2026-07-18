// 角色编辑器面板 —— fetch /api/roles/config 渲染表单，保存时 POST
// 回后端（重写 .latte/agents.d/<id>.toml + prompt 文件）。打开方式：
// 顶栏「⚙️ Roles」按钮，或在聊天消息的角色头像上右键（chat_impl.ts
// 的 onEditRole 回调）。

import {
  getRolesConfig,
  saveRoleConfig,
} from "./api";
import type {
  RoleConfigEntry,
  RolesConfig,
} from "./api";

interface UIBinding {
  panelEl: HTMLElement;
  roleSelect: HTMLSelectElement;
  closeBtn: HTMLButtonElement;
  refreshBtn: HTMLButtonElement;
  formEl: HTMLFormElement;
  nameInput: HTMLInputElement;
  iconInput: HTMLInputElement;
  tierSelect: HTMLSelectElement;
  chainInput: HTMLInputElement;
  temperatureInput: HTMLInputElement;
  toolsEl: HTMLElement;
  promptInput: HTMLTextAreaElement;
  statusEl: HTMLElement;
}

export interface RoleEditorController {
  isOpen(): boolean;
  /** 打开面板；带 roleId 时直接跳到该角色。 */
  open(roleId?: string): void;
}

export function mountRoleEditor(opts: {
  container: UIBinding;
}): RoleEditorController {
  const { container } = opts;
  let config: RolesConfig | null = null;
  let isLoading = false;
  let isSaving = false;

  container.closeBtn.addEventListener("click", () => {
    container.panelEl.classList.add("hidden");
  });
  container.refreshBtn.addEventListener("click", () => {
    void refresh(currentRoleId());
  });
  container.roleSelect.addEventListener("change", () => {
    fillForm(currentRoleId());
  });
  container.formEl.addEventListener("submit", (e) => {
    e.preventDefault();
    void save();
  });

  function currentRoleId(): string {
    return container.roleSelect.value;
  }

  function currentEntry(): RoleConfigEntry | null {
    if (!config) return null;
    return config.roles.find((r) => r.id === currentRoleId()) ?? null;
  }

  function setStatus(msg: string, isError = false): void {
    container.statusEl.textContent = msg;
    container.statusEl.classList.toggle("error", isError);
  }

  async function refresh(preferRoleId?: string): Promise<void> {
    if (isLoading) return;
    isLoading = true;
    setStatus("加载中…");
    try {
      config = await getRolesConfig();
      renderRoleSelect(preferRoleId);
      renderTierSelect();
      fillForm(currentRoleId());
      setStatus("");
    } catch (e) {
      setStatus(`加载失败: ${(e as Error).message}`, true);
    } finally {
      isLoading = false;
    }
  }

  function renderRoleSelect(preferRoleId?: string): void {
    container.roleSelect.innerHTML = "";
    for (const r of config?.roles ?? []) {
      const opt = document.createElement("option");
      opt.value = r.id;
      opt.textContent = `${r.icon} ${r.id} — ${r.name}`;
      container.roleSelect.appendChild(opt);
    }
    if (
      preferRoleId &&
      config?.roles.some((r) => r.id === preferRoleId)
    ) {
      container.roleSelect.value = preferRoleId;
    }
  }

  function renderTierSelect(): void {
    container.tierSelect.innerHTML = "";
    for (const t of config?.tiers ?? []) {
      const opt = document.createElement("option");
      opt.value = t;
      opt.textContent = t;
      container.tierSelect.appendChild(opt);
    }
  }

  function fillForm(roleId: string): void {
    const entry = config?.roles.find((r) => r.id === roleId);
    if (!entry) return;
    container.nameInput.value = entry.name;
    container.iconInput.value = entry.icon;
    container.tierSelect.value = entry.model_tier;
    container.chainInput.value = entry.model_chain.join(", ");
    container.temperatureInput.value =
      entry.temperature === null ? "" : String(entry.temperature);
    container.promptInput.value = entry.prompt;
    renderTools(entry);
    setStatus("");
  }

  function renderTools(entry: RoleConfigEntry): void {
    container.toolsEl.innerHTML = "";
    const selected = new Set(entry.tools);
    for (const name of config?.available_tools ?? []) {
      const label = document.createElement("label");
      label.className = "role-editor-tool";
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.value = name;
      cb.checked = selected.has(name);
      label.appendChild(cb);
      label.appendChild(document.createTextNode(name));
      container.toolsEl.appendChild(label);
    }
  }

  function checkedTools(): string[] {
    const out: string[] = [];
    for (const cb of container.toolsEl.querySelectorAll(
      'input[type="checkbox"]',
    )) {
      const input = cb as HTMLInputElement;
      if (input.checked) out.push(input.value);
    }
    return out;
  }

  async function save(): Promise<void> {
    const entry = currentEntry();
    if (!entry || isSaving) return;
    isSaving = true;
    setStatus("保存中…");
    try {
      const temperature = container.temperatureInput.value.trim();
      const updated = await saveRoleConfig({
        id: entry.id,
        name: container.nameInput.value.trim(),
        icon: container.iconInput.value.trim(),
        model_tier: container.tierSelect.value,
        model_chain: container.chainInput.value
          .split(",")
          .map((s) => s.trim())
          .filter((s) => s.length > 0),
        temperature: temperature === "" ? null : Number(temperature),
        tools: checkedTools(),
        prompt: container.promptInput.value,
      });
      // 更新本地缓存，保持 select 里显示的名称同步。
      if (config) {
        const idx = config.roles.findIndex((r) => r.id === updated.id);
        if (idx >= 0) config.roles[idx] = updated;
      }
      renderRoleSelect(updated.id);
      fillForm(updated.id);
      setStatus("✅ 已保存（新 session 生效）");
    } catch (e) {
      setStatus(`保存失败: ${(e as Error).message}`, true);
    } finally {
      isSaving = false;
    }
  }

  return {
    isOpen: () => !container.panelEl.classList.contains("hidden"),
    open: (roleId?: string) => {
      container.panelEl.classList.remove("hidden");
      void refresh(roleId);
    },
  };
}
