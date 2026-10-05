// 模型账户余额不足横幅：后端把余额不足 / 配额耗尽的账户从所有模型链里摘除
// （latte-agent-core/src/billing.rs），这里常驻显示「哪个账户没钱、哪些
// 角色在用它、哪些角色已经整条链不可用」。数据来自 GET /api/billing；
// 收到 💳 状态消息、恢复（▶）后各刷新一次，挂载时也拉一次。

import { getTransport } from "./transport";

export interface RoleHit {
  role: string;
  models: string[];
  all_blocked: boolean;
}
export interface BlockedAccount {
  provider: string;
  models: string[];
  message: string;
  recheck_in_secs: number;
  roles: RoleHit[];
}

export async function fetchBilling(): Promise<BlockedAccount[]> {
  const r = await getTransport().request<{ accounts: BlockedAccount[] }>("GET", "/api/billing");
  return r?.accounts ?? [];
}

/** 渲染（或移除）横幅。`anchor` 之前插入；无账户时移除。安全：全部 textContent。 */
export function renderBillingBanner(anchor: HTMLElement, accounts: BlockedAccount[]): void {
  const parent = anchor.parentElement;
  if (!parent) return;
  let el = parent.querySelector<HTMLElement>(":scope > .billing-banner");
  if (accounts.length === 0) {
    el?.remove();
    return;
  }
  if (!el) {
    el = document.createElement("div");
    el.className = "billing-banner";
    el.setAttribute("role", "alert");
    parent.insertBefore(el, anchor);
  }
  el.replaceChildren();
  for (const a of accounts) {
    const row = document.createElement("div");
    row.className = "billing-banner__row";
    const head = document.createElement("div");
    head.className = "billing-banner__head";
    head.textContent = `💳 ${a.provider} 账户余额不足 / 配额耗尽：${a.models.join("、")} 已从所有模型链摘除，不再重试`;
    row.appendChild(head);
    const roles = document.createElement("div");
    roles.className = "billing-banner__roles";
    if (a.roles.length === 0) {
      roles.textContent = "当前没有角色的模型链用到它。";
    } else {
      roles.append("受影响角色：");
      a.roles.forEach((r, i) => {
        const chip = document.createElement("span");
        chip.className = "billing-banner__chip" + (r.all_blocked ? " is-dead" : "");
        chip.textContent = `${r.role}（${r.models.join("、")}${r.all_blocked ? "，整条链不可用" : "，会回落到链上其它模型"}）`;
        chip.title = r.all_blocked
          ? "这个角色链上的所有模型都没钱了，派给它的任务会暂停等你处理"
          : "这个角色还有其它可用模型，会自动跳过没钱的账户";
        roles.appendChild(chip);
        if (i < a.roles.length - 1) roles.append(" ");
      });
    }
    row.appendChild(roles);
    const tip = document.createElement("div");
    tip.className = "billing-banner__tip";
    tip.textContent = `充值后点 ▶ 继续（或 /resume）立即重新检测；${Math.ceil(a.recheck_in_secs / 60)} 分钟后也会自动探测。原始报错：${a.message}`;
    row.appendChild(tip);
    el.appendChild(row);
  }
}

/** 拉取并渲染；失败静默（横幅是辅助信息，不该打断聊天）。 */
export async function refreshBillingBanner(anchor: HTMLElement): Promise<void> {
  try {
    renderBillingBanner(anchor, await fetchBilling());
  } catch {
    /* 旧后端没有 /api/billing：不显示 */
  }
}
