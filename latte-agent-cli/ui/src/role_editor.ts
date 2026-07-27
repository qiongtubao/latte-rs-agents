// 角色编辑器面板 —— fetch /api/roles/config 渲染表单，保存时 POST。
// 支持两种编辑方式：表单编辑（结构化）和源文件编辑（TOML 原始内容）。
// 支持模型链浏览（联动模型管理面板）、工具跳转（联动工具管理页面）和角色测试。
import { getRolesConfig, saveRoleConfig, createRole, deleteRole, testRole, getRoleToml, putRoleToml } from "./api";
import type { RoleConfigEntry, RolesConfig, TestRoleResponse } from "./api";

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
  chainBrowseBtn: HTMLButtonElement;
  temperatureInput: HTMLInputElement;
  toolsEl: HTMLElement;
  codePathsInput: HTMLTextAreaElement;
  promptInput: HTMLTextAreaElement;
  statusEl: HTMLElement;
  testRoleBtn: HTMLButtonElement;
  pathsEl: HTMLElement;
  // TOML 源文件编辑
  tabBarEl: HTMLElement;
  tabBtns: NodeListOf<HTMLButtonElement>;
  formPane: HTMLElement;
  tomlPane: HTMLElement;
  tomlEditor: HTMLTextAreaElement;
  tomlSaveBtn: HTMLButtonElement;
  tomlReloadBtn: HTMLButtonElement;
  tomlStatusEl: HTMLElement;
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
  // 当前选中的角色 id（可能和 roleSelect.value 不同步——TOML tab 切换不触发 fillForm）
  let currentRoleId = "";

  // ── 表单编辑事件 ──
  container.closeBtn.addEventListener("click", () => container.panelEl.classList.add("hidden"));
  container.refreshBtn.addEventListener("click", () => void refresh(container.roleSelect.value));
  container.roleSelect.addEventListener("change", () => {
    currentRoleId = container.roleSelect.value;
    if (container.formPane.classList.contains("active")) {
      fillForm(currentRoleId);
    } else if (container.tomlPane.classList.contains("active") && currentRoleId) {
      void loadToml(currentRoleId);
    }
  });
  container.formEl.addEventListener("submit", (event) => {
    event.preventDefault();
    void save();
  });
  container.newBtn.addEventListener("click", () => void onCreate());
  container.deleteBtn.addEventListener("click", () => void onDelete());
  container.testRoleBtn.addEventListener("click", () => void onTestRole());

  // ── tab 切换 ──
  const tabBtns = Array.from(container.tabBtns);
  tabBtns.forEach((btn) => {
    btn.addEventListener("click", () => {
      const target = btn.dataset.tab;
      if (!target) return;
      tabBtns.forEach((b) => b.classList.toggle("active", b === btn));
      container.formPane.classList.toggle("active", target === "form");
      container.tomlPane.classList.toggle("active", target === "toml");
      // 切到 TOML tab 时加载当前角色的源文件
      if (target === "toml" && currentRoleId) {
        void loadToml(currentRoleId);
      }
    });
  });

  // ── TOML 编辑事件 ──
  container.tomlSaveBtn.addEventListener("click", () => void saveToml());
  container.tomlReloadBtn.addEventListener("click", () => {
    if (currentRoleId) void loadToml(currentRoleId);
  });

  // ── 状态提示 ──
  const setStatus = (message: string, error = false) => {
    container.statusEl.textContent = message;
    container.statusEl.classList.toggle("error", error);
  };
  const setTomlStatus = (message: string, error = false) => {
    container.tomlStatusEl.textContent = message;
    container.tomlStatusEl.classList.toggle("error", error);
  };

  const chainValues = () => Array.from(container.chainEl.querySelectorAll<HTMLInputElement>("input"))
    .map((input) => input.value.trim())
    .filter(Boolean);

  // 代码/文档路径：textarea 每行一条，忽略空行。
  const codePathValues = () => container.codePathsInput.value
    .split("\n")
    .map((line) => line.trim())
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
        button("🔍", "在模型管理中查看此模型", () => {
          const modelId = input.value.trim();
          if (modelId) {
            container.chainBrowseBtn.dispatchEvent(
              new CustomEvent("select-model", { bubbles: true, detail: { modelId } })
            );
          }
        }),
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
    currentRoleId = roleId;
    container.nameInput.value = entry.name;
    container.iconInput.value = entry.icon;
    container.tierSelect.value = entry.model_tier;
    renderChain(entry.model_chain);
    container.temperatureInput.value = entry.temperature === null ? "" : String(entry.temperature);
    container.codePathsInput.value = (entry.code_paths ?? []).join("\n");
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
      const jump = document.createElement("button");
      jump.type = "button";
      jump.className = "role-editor-tool-jump";
      jump.textContent = "🔍";
      jump.title = "在工具管理中查看此工具";
      jump.addEventListener("click", (event) => {
        event.preventDefault(); // 避免触发 label 默认勾选 checkbox
        container.toolsEl.dispatchEvent(
          new CustomEvent("select-tool", { bubbles: true, detail: { toolId: name } })
        );
      });
      label.append(checkbox, document.createTextNode(name), jump);
      container.toolsEl.appendChild(label);
    }
    setStatus("");
  }

  /** 加载角色 TOML 源文件到编辑器。 */
  async function loadToml(roleId: string): Promise<void> {
    setTomlStatus("加载中…");
    container.tomlEditor.disabled = true;
    try {
      const raw = await getRoleToml(roleId);
      container.tomlEditor.value = raw;
      setTomlStatus("（只读加载，修改后点「保存 TOML」写盘）");
    } catch (err) {
      setTomlStatus(`加载失败: ${(err as Error).message}`, true);
    } finally {
      container.tomlEditor.disabled = false;
    }
  }

  /** 保存 TOML 源文件。 */
  async function saveToml(): Promise<void> {
    if (isSaving || !currentRoleId) return;
    isSaving = true;
    setTomlStatus("保存中…");
    container.tomlSaveBtn.disabled = true;
    try {
      await putRoleToml(currentRoleId, container.tomlEditor.value);
      setTomlStatus("✅ TOML 已保存；新 session 生效");
      // 刷新 config 和表单
      await refresh(currentRoleId);
    } catch (err) {
      setTomlStatus(`保存失败: ${(err as Error).message}`, true);
    } finally {
      isSaving = false;
      container.tomlSaveBtn.disabled = false;
    }
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
      currentRoleId = container.roleSelect.value;
      container.tierSelect.replaceChildren();
      for (const tier of config.tiers) {
        const option = document.createElement("option");
        option.value = tier;
        option.textContent = tier;
        container.tierSelect.appendChild(option);
      }
      fillForm(currentRoleId);
      // 如果当前在 TOML tab，重新加载 TOML
      if (container.tomlPane.classList.contains("active") && currentRoleId) {
        void loadToml(currentRoleId);
      }
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
        code_paths: codePathValues(),
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

  async function onTestRole(): Promise<void> {
    const entry = config?.roles.find((role) => role.id === container.roleSelect.value);
    if (!entry) {
      setStatus("请先选择一个角色", true);
      return;
    }
    setStatus("测试中…");
    container.testRoleBtn.disabled = true;
    try {
      const resp = await testRole({
        role_id: entry.id,
        config: {
          id: entry.id,
          name: container.nameInput.value.trim(),
          icon: container.iconInput.value.trim(),
          model_tier: container.tierSelect.value,
          model_chain: chainValues(),
          temperature: container.temperatureInput.value.trim() === "" ? null : Number(container.temperatureInput.value.trim()),
          tools: Array.from(container.toolsEl.querySelectorAll<HTMLInputElement>('input[type="checkbox"]'))
            .filter((input) => input.checked)
            .map((input) => input.value),
          code_paths: codePathValues(),
          prompt: container.promptInput.value,
        },
      });
      setStatus(resp.ok ? `✅ 角色测试通过 (${resp.latency_ms}ms)` : `✗ 角色测试失败: ${resp.error ?? "未知错误"}`, !resp.ok);
    } catch (err) {
      setStatus(`测试失败: ${(err as Error).message}`, true);
    } finally {
      container.testRoleBtn.disabled = false;
    }
  }

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
      if (config) {
        config.roles.push(entry);
      }
      await refresh(roleId);
      setStatus(`已创建角色 "${entry.id}"`);
    } catch (err) {
      setStatus(`创建失败: ${(err as Error).message}`, true);
    }
  }

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
      container.roleSelect.replaceChildren();
      for (const role of config?.roles ?? []) {
        const option = document.createElement("option");
        option.value = role.id;
        option.textContent = `${role.icon} ${role.id} — ${role.name}`;
        container.roleSelect.appendChild(option);
      }
      if (container.roleSelect.options.length > 0) {
        container.roleSelect.selectedIndex = 0;
        currentRoleId = container.roleSelect.value;
        fillForm(currentRoleId);
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
