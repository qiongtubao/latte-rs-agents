// 角色编辑器面板 —— fetch /api/roles/config 渲染表单，保存时 POST。
import { getRolesConfig, saveRoleConfig, createRole, deleteRole } from "./api";
import type { RoleConfigEntry, RolesConfig } from "./api";
interface UIBinding {
  panelEl: HTMLElement;
  roleSelect: HTMLSelectElement;
  closeBtn: HTMLButtonElement;
  refreshBtn: HTMLButtonElement;
  newBtn: HTMLButtonElement;
  deleteBtn: HTMLButtonElement;
  formEl: HTMLFormElement;
  nameInput: HTMLInputElement;
  iconInput: HTMLInputElement;
  tierSelect: HTMLSelectElement;
  chainEl: HTMLElement;
  temperatureInput: HTMLInputElement;
  toolsEl: HTMLElement;
  promptInput: HTMLTextAreaElement;
  statusEl: HTMLElement;
  pathsEl: HTMLElement;
}

export interface RoleEditorController {
  isOpen(): boolean;
  open(roleId?: string): void;
}

export function mountRoleEditor(opts: { container: UIBinding }): RoleEditorController {
  const { container } = opts;
  let config: RolesConfig | null = null;
  let isLoading = false;
  let isSaving = false;

  container.closeBtn.addEventListener("click", () => container.panelEl.classList.add("hidden"));
  container.refreshBtn.addEventListener("click", () => void refresh(container.roleSelect.value));
  container.roleSelect.addEventListener("change", () => fillForm(container.roleSelect.value));
  container.formEl.addEventListener("submit", (event) => {
    event.preventDefault();
    void save();
  });
  container.newBtn.addEventListener("click", () => void onCreate());
  container.deleteBtn.addEventListener("click", () => void onDelete());

  const setStatus = (message: string, error = false) => {
    container.statusEl.textContent = message;
    container.statusEl.classList.toggle("error", error);
  };

  const chainValues = () => Array.from(container.chainEl.querySelectorAll<HTMLInputElement>("input"))
    .map((input) => input.value.trim())
    .filter(Boolean);

  function renderChain(models: string[]): void {
    container.chainEl.replaceChildren();
    const values = models.length ? models : [""];
    values.forEach((model, index) => {
      const row = document.createElement("div");
      row.className = "role-editor-chain-row";
      const order = document.createElement("span");
      order.className = "role-editor-chain-order";
      order.textContent = `${index + 1}.`;
      const input = document.createElement("input");
      input.type = "text";
      input.value = model;
      input.placeholder = "model-id";
      const button = (label: string, title: string, action: () => void) => {
        const el = document.createElement("button");
        el.type = "button";
        el.textContent = label;
        el.title = title;
        el.addEventListener("click", action);
        return el;
      };
      row.append(
        order,
        input,
        button("↑", "提高优先级", () => {
          const next = chainValues();
          if (index > 0) [next[index - 1], next[index]] = [next[index], next[index - 1]];
          renderChain(next);
        }),
        button("↓", "降低优先级", () => {
          const next = chainValues();
          if (index < next.length - 1) [next[index], next[index + 1]] = [next[index + 1], next[index]];
          renderChain(next);
        }),
        button("×", "删除模型", () => {
          const next = chainValues();
          next.splice(index, 1);
          renderChain(next);
        }),
      );
      container.chainEl.appendChild(row);
    });
    const add = document.createElement("button");
    add.type = "button";
    add.className = "role-editor-chain-add";
    add.textContent = "+ 添加模型";
    add.addEventListener("click", () => renderChain([...chainValues(), ""]));
    container.chainEl.appendChild(add);
  }

  function fillForm(roleId: string): void {
    const entry = config?.roles.find((role) => role.id === roleId);
    if (!entry) return;
    container.nameInput.value = entry.name;
    container.iconInput.value = entry.icon;
    container.tierSelect.value = entry.model_tier;
    renderChain(entry.model_chain);
    container.temperatureInput.value = entry.temperature === null ? "" : String(entry.temperature);
    container.promptInput.value = entry.prompt;
    container.pathsEl.textContent = `配置文件：${entry.config_path}${entry.prompt_path ? ` · Prompt：${entry.prompt_path}` : ""}`;
    container.toolsEl.replaceChildren();
    const selected = new Set(entry.tools);
    for (const name of config?.available_tools ?? []) {
      const label = document.createElement("label");
      label.className = "role-editor-tool";
      const checkbox = document.createElement("input");
      checkbox.type = "checkbox";
      checkbox.value = name;
      checkbox.checked = selected.has(name);
      label.append(checkbox, document.createTextNode(name));
      container.toolsEl.appendChild(label);
    }
    setStatus("");
  }

  async function refresh(preferRoleId?: string): Promise<void> {
    if (isLoading) return;
    isLoading = true;
    setStatus("加载中…");
    try {
      config = await getRolesConfig();
      container.roleSelect.replaceChildren();
      for (const role of config.roles) {
        const option = document.createElement("option");
        option.value = role.id;
        option.textContent = `${role.icon} ${role.id} — ${role.name}`;
        container.roleSelect.appendChild(option);
      }
      if (preferRoleId && config.roles.some((role) => role.id === preferRoleId)) {
        container.roleSelect.value = preferRoleId;
      }
      container.tierSelect.replaceChildren();
      for (const tier of config.tiers) {
        const option = document.createElement("option");
        option.value = tier;
        option.textContent = tier;
        container.tierSelect.appendChild(option);
      }
      fillForm(container.roleSelect.value);
    } catch (error) {
      setStatus(`加载失败: ${(error as Error).message}`, true);
    } finally {
      isLoading = false;
    }
  }

  async function save(): Promise<void> {
    const entry = config?.roles.find((role) => role.id === container.roleSelect.value);
    if (!entry || isSaving) return;
    isSaving = true;
    setStatus("保存中…");
    try {
      const rawTemperature = container.temperatureInput.value.trim();
      const updated = await saveRoleConfig({
        id: entry.id,
        name: container.nameInput.value.trim(),
        icon: container.iconInput.value.trim(),
        model_tier: container.tierSelect.value,
        model_chain: chainValues(),
        temperature: rawTemperature === "" ? null : Number(rawTemperature),
        tools: Array.from(container.toolsEl.querySelectorAll<HTMLInputElement>('input[type="checkbox"]'))
          .filter((input) => input.checked)
          .map((input) => input.value),
        prompt: container.promptInput.value,
      });
      if (config) {
        const index = config.roles.findIndex((role) => role.id === updated.id);
        if (index >= 0) config.roles[index] = updated;
      }
      fillForm(updated.id);
      setStatus("✅ 已保存到配置；新 session 生效");
    } catch (error) {
      setStatus(`保存失败: ${(error as Error).message}`, true);
    } finally {
      isSaving = false;
    }
  }

  /** 新建角色：弹出 prompt 输入 id，POST /api/roles 后刷新列表。 */
  async function onCreate(): Promise<void> {
    const roleId = prompt("新角色 ID（字母/数字/下划线）：");
    if (!roleId) return;
    if (!/^[a-zA-Z0-9_]+$/.test(roleId)) {
      setStatus("角色 ID 只能包含字母/数字/下划线", true);
      return;
    }
    const name = prompt("角色显示名：", roleId);
    if (!name) return;
    try {
      setStatus("创建中…");
      const entry = await createRole({ id: roleId, name });
      // 追加到本地 config 并刷新下拉菜单
      if (config) {
        config.roles.push(entry);
      }
      await refresh(roleId);
      setStatus(`已创建角色 "${entry.id}"`);
    } catch (err) {
      setStatus(`创建失败: ${(err as Error).message}`, true);
    }
  }

  /** 删除角色：二次确认后 DELETE /api/roles/:id，刷新列表。 */
  async function onDelete(): Promise<void> {
    const roleId = container.roleSelect.value;
    if (!roleId) return;
    if (!confirm(`确定删除角色 "${roleId}"？此操作不可撤销，且会删除对应 agents.d 文件。`)) return;
    try {
      setStatus("删除中…");
      await deleteRole(roleId);
      if (config) {
        config.roles = config.roles.filter(r => r.id !== roleId);
      }
      setStatus(`已删除角色 "${roleId}"`);
      // 重建下拉菜单，跳到下一个可用角色
      container.roleSelect.replaceChildren();
      for (const role of config?.roles ?? []) {
        const option = document.createElement("option");
        option.value = role.id;
        option.textContent = `${role.icon} ${role.id} — ${role.name}`;
        container.roleSelect.appendChild(option);
      }
      if (container.roleSelect.options.length > 0) {
        container.roleSelect.selectedIndex = 0;
        fillForm(container.roleSelect.value);
      } else {
        container.formEl.classList.add("hidden");
      }
    } catch (err) {
      setStatus(`删除失败: ${(err as Error).message}`, true);
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
