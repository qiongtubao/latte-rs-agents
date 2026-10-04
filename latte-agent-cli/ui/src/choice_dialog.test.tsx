// ask 选择框（ChoiceRequested）的弹窗 / 多选 / 详情行为测试。
//
// 背景：选择框此前只内联渲染在消息流末尾。会话被 ask 阻塞时，用户
// 不下拉到底根本看不出"在等我"，弹框形同没弹。现在卡片建在消息流里
// （历史留痕）但立刻搬进居中弹窗；收起后右下角留待办铃可重新展开。
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mountChat } from "./chat_impl";
import type { ChatEvent, ChoiceOption } from "./api";

// 提交路径会打网络：桩掉，只断言 UI 行为与回传文本。
const sent: string[] = [];
const answered: Array<{ choiceId: string; answer: string; questionId?: string }> = [];
vi.mock("./api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./api")>();
  return {
    ...actual,
    sendMessage: (msg: string) => { sent.push(msg); return Promise.resolve(); },
    sendChoiceAnswer: (choiceId: string, answer: string, questionId?: string) => {
      answered.push({ choiceId, answer, questionId });
      return Promise.resolve(true);
    },
    listWorkflows: () => Promise.resolve([]),
    listTasks: () => Promise.resolve([]),
  };
});

function mount() {
  document.body.replaceChildren();
  const el = <T extends HTMLElement>(tag: string) => document.createElement(tag) as unknown as T;
  const statusPill = el<HTMLElement>("div");
  const statusLabel = el<HTMLElement>("span");
  statusLabel.className = "status-label";
  const statusTime = el<HTMLElement>("span");
  statusTime.className = "status-time";
  statusPill.append(statusLabel, statusTime);
  const container = {
    messagesEl: el<HTMLElement>("div"),
    formEl: el<HTMLFormElement>("form"),
    inputEl: el<HTMLTextAreaElement>("textarea"),
    sendBtn: el<HTMLButtonElement>("button"),
    clearBtn: el<HTMLButtonElement>("button"),
    quitBtn: el<HTMLButtonElement>("button"),
    pauseBtn: el<HTMLButtonElement>("button"),
    resumeBtn: el<HTMLButtonElement>("button"),
    statusPill,
    roleSelect: el<HTMLSelectElement>("select"),
    rolePauseToggle: el<HTMLButtonElement>("button"),
    rolePill: el<HTMLElement>("div"),
    modelPill: el<HTMLElement>("div"),
    footerMsg: el<HTMLElement>("div"),
  };
  document.body.appendChild(container.messagesEl);
  for (const id of [
    "cmd-autocomplete", "role-autocomplete", "contextMenu", "editOverlay",
    "editTextarea", "editSave", "editCancel", "editCancelTop", "subagentLog",
    "subagentOverlay",
  ]) {
    const node = document.createElement(id === "editTextarea" ? "textarea" : "div");
    node.id = id;
    document.body.appendChild(node);
  }
  const chat = mountChat({ container, initialRole: "manager" });
  return { chat, messagesEl: container.messagesEl };
}

function choiceEvent(over: Partial<Extract<ChatEvent, { type: "ChoiceRequested" }>> = {}): ChatEvent {
  const options: ChoiceOption[] = [
    { label: "JWT", description: "无状态 token", pros: ["无状态", "客户端友好"], cons: ["撤销难"] },
    { label: "Session", description: "服务端存会话" },
  ];
  return {
    type: "ChoiceRequested",
    role_id: "architect",
    choice_id: "choice-architect-0",
    question: "鉴权方案选哪个？",
    multi: false,
    layout: "",
    allow_upload: false,
    wait: true,
    options,
    ...over,
  } as ChatEvent;
}

const overlay = () => document.querySelector(".choice-overlay");
const cardInOverlay = () => document.querySelector(".choice-overlay .choice-card");
const pill = () => document.querySelector<HTMLElement>(".choice-pending-pill");
/** 断言一律针对**弹窗里**那张卡片：消息流里可能还躺着排队中的卡片，
 *  不限定作用域会误点到队列里的另一道题。 */
const scope = () => document.querySelector<HTMLElement>(".choice-overlay") ?? document.body;
const opts = () => Array.from(scope().querySelectorAll<HTMLElement>(".choice-card .choice-opt"));
const submitBtn = () => scope().querySelector<HTMLButtonElement>(".choice-card .choice-submit")!;

beforeEach(() => { sent.length = 0; answered.length = 0; });

describe("ask 选择框弹窗", () => {
  it("ChoiceRequested 立刻弹出居中弹窗（不必下拉到消息流底部）", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(choiceEvent());
    expect(overlay(), "应有弹窗遮罩").toBeTruthy();
    expect(cardInOverlay(), "卡片应被搬进弹窗").toBeTruthy();
    // 消息流里仍留有那条系统提示（历史留痕）+ 展开入口占位。
    expect(messagesEl.textContent).toContain("鉴权方案选哪个？");
    expect(messagesEl.querySelector(".choice-moved-hint")).toBeTruthy();
    // 阻塞中的 ask 要显式标出来。
    expect(document.querySelector(".choice-modal__blocked")).toBeTruthy();
  });

  it("收起（Esc）后卡片回到消息流并留下待办铃，点铃重新弹出", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(choiceEvent());
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(overlay()).toBeNull();
    expect(messagesEl.querySelector(".choice-card"), "卡片应归位消息流").toBeTruthy();
    const p = pill();
    expect(p?.textContent).toContain("1 个选择等你回答");
    p!.click();
    expect(overlay()).toBeTruthy();
    expect(pill(), "弹窗已显示，铃应消失").toBeNull();
  });

  it("单选：选中即可提交，答案经 choice-answer 直达阻塞方，弹窗关闭", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(choiceEvent());
    expect(submitBtn().disabled).toBe(true);
    opts()[0].click();
    expect(submitBtn().disabled).toBe(false);
    submitBtn().click();
    expect(answered).toEqual([{ choiceId: "choice-architect-0", answer: "我的选择：JWT" }]);
    expect(sent, "wait=true 不该另起一轮 user 消息").toHaveLength(0);
    expect(overlay(), "答完应关弹窗").toBeNull();
    expect(pill(), "没有待答项则无铃").toBeNull();
    expect(messagesEl.querySelector(".choice-card.answered")).toBeTruthy();
    expect(messagesEl.textContent).toContain("你的选择：JWT");
  });

  it("多选：可勾选多项，提交按钮显示项数，答案合并回传", () => {
    const { chat } = mount();
    chat.handleEvent(choiceEvent({ multi: true, wait: false }));
    expect(document.querySelector(".choice-chip")?.textContent).toContain("多选");
    expect(opts()[0].className).toContain("check");
    opts()[0].click();
    opts()[1].click();
    expect(opts().filter((o) => o.classList.contains("sel"))).toHaveLength(2);
    expect(submitBtn().textContent).toContain("2 项");
    submitBtn().click();
    // wait=false（顶层 turn）→ 走普通 user 消息回喂。
    expect(sent).toEqual(["我的选择：JWT、Session"]);
  });

  it("单选点第二项会替换第一项（互斥）", () => {
    const { chat } = mount();
    chat.handleEvent(choiceEvent());
    opts()[0].click();
    opts()[1].click();
    const sel = opts().filter((o) => o.classList.contains("sel"));
    expect(sel).toHaveLength(1);
    expect(sel[0].textContent).toContain("Session");
  });

  it("详情按钮展开优缺点，且不会顺手把该项选中", () => {
    const { chat } = mount();
    chat.handleEvent(choiceEvent());
    const first = opts()[0];
    const btn = first.querySelector<HTMLButtonElement>(".choice-detail-btn")!;
    const panel = first.querySelector<HTMLElement>(".choice-detail")!;
    expect(panel.hidden, "默认收起").toBe(true);
    btn.click();
    expect(panel.hidden).toBe(false);
    expect(panel.textContent).toContain("优点");
    expect(panel.textContent).toContain("无状态");
    expect(panel.textContent).toContain("撤销难");
    expect(first.classList.contains("sel"), "展开详情不等于选中").toBe(false);
    btn.click();
    expect(panel.hidden, "再点收起").toBe(true);
    // 没给 pros/cons 的项不长出详情按钮。
    expect(opts()[1].querySelector(".choice-detail-btn")).toBeNull();
  });

  it("两个待答选择排队：先弹一个，答完自动弹下一个", () => {
    const { chat } = mount();
    chat.handleEvent(choiceEvent());
    chat.handleEvent(choiceEvent({ choice_id: "choice-architect-1", question: "第二问？" }));
    // 同时只弹一个，另一个进铃。
    expect(document.querySelectorAll(".choice-overlay")).toHaveLength(1);
    expect(document.querySelector(".choice-modal__title")?.textContent).toContain("architect");
    expect(pill()?.textContent).toContain("1 个选择等你回答");
    opts()[0].click();
    submitBtn().click();
    expect(overlay(), "答完第一个应自动弹第二个").toBeTruthy();
    expect(document.querySelector(".choice-overlay")?.textContent).toContain("第二问？");
    expect(pill()).toBeNull();
  });

  it("历史回放不弹窗、不留铃（不冒出僵尸弹框）", () => {
    const { chat, messagesEl } = mount();
    chat.replayEvents([choiceEvent()]);
    expect(overlay()).toBeNull();
    expect(pill()).toBeNull();
    expect(messagesEl.querySelector(".choice-card"), "历史卡片仍在消息流里可见").toBeTruthy();
  });

  it("切 session（clear）清掉残留弹窗与待办铃", () => {
    const { chat } = mount();
    chat.handleEvent(choiceEvent());
    expect(overlay()).toBeTruthy();
    chat.clear();
    expect(overlay()).toBeNull();
    expect(pill()).toBeNull();
  });

  // ─── 多题弹框：一次问 N 道、全部答完才能提交 ─────────────────────
  //
  // 回归 2026-09-07 jemalloc 会话：manager 一轮连发 3 道 ask，用户答完
  // 前 2 道它就启动了 4 条 workflow，剩下 2 道的答案在流水线跑完之后
  // 才到 —— 前面全按默认假设做了。
  function multiEvent(wait: boolean): ChatEvent {
    return choiceEvent({
      choice_id: "choice-manager-multi",
      wait,
      questions: [
        {
          id: "q1",
          question: "你的背景？",
          multi: false,
          options: [{ label: "小白" }, { label: "有基础" }],
        },
        {
          id: "q2",
          question: "产出形式？",
          multi: false,
          options: [{ label: "只要清单" }, { label: "清单+教程" }],
        },
        {
          id: "q3",
          question: "深度档位？",
          multi: false,
          options: [{ label: "读懂主链路" }, { label: "改得动" }],
        },
      ],
    } as Partial<Extract<ChatEvent, { type: "ChoiceRequested" }>>);
  }

  const qBlocks = () => Array.from(scope().querySelectorAll<HTMLElement>(".choice-q-block"));

  it("多题：渲染成一个弹框，答不全不能提交", () => {
    const { chat } = mount();
    chat.handleEvent(multiEvent(true));
    expect(overlay()).not.toBeNull();
    expect(qBlocks()).toHaveLength(3);
    expect(submitBtn().disabled).toBe(true);

    // 只答第 1 题 —— 这正是实测里 manager 提前开工的那一刻
    const q1opt = qBlocks()[0].querySelectorAll<HTMLElement>(".choice-opt")[0];
    q1opt.click();
    expect(q1opt.classList.contains("sel"), "选中项要有 sel 高亮态").toBe(true);
    expect(submitBtn().disabled).toBe(true);
    // 答第 2 题，仍不能提交
    qBlocks()[1].querySelectorAll<HTMLElement>(".choice-opt")[1].click();
    expect(submitBtn().disabled).toBe(true);
    expect(scope().querySelector(".choice-status")!.textContent).toContain("2 / 3");

    // 答满 3 题才放行
    qBlocks()[2].querySelectorAll<HTMLElement>(".choice-opt")[1].click();
    expect(submitBtn().disabled).toBe(false);
  });

  it("多题（阻塞）：提交时逐题投递，带上各自的 question_id", async () => {
    const { chat } = mount();
    chat.handleEvent(multiEvent(true));
    qBlocks()[0].querySelectorAll<HTMLElement>(".choice-opt")[0].click();
    qBlocks()[1].querySelectorAll<HTMLElement>(".choice-opt")[1].click();
    qBlocks()[2].querySelectorAll<HTMLElement>(".choice-opt")[1].click();
    submitBtn().click();
    await Promise.resolve();
    await Promise.resolve();
    expect(answered.map((a) => a.questionId)).toEqual(["q1", "q2", "q3"]);
    expect(answered.map((a) => a.answer)).toEqual(["小白", "清单+教程", "改得动"]);
    // 阻塞路径不该再走普通消息
    expect(sent).toHaveLength(0);
    expect(overlay()).toBeNull();
  });

  it("多题（fire-and-forget）：N 个答案合成**一条** user 消息，只起一个 turn", async () => {
    const { chat } = mount();
    chat.handleEvent(multiEvent(false));
    qBlocks()[0].querySelectorAll<HTMLElement>(".choice-opt")[0].click();
    qBlocks()[1].querySelectorAll<HTMLElement>(".choice-opt")[0].click();
    qBlocks()[2].querySelectorAll<HTMLElement>(".choice-opt")[0].click();
    submitBtn().click();
    await Promise.resolve();
    expect(sent).toHaveLength(1);
    expect(sent[0]).toContain("小白");
    expect(sent[0]).toContain("只要清单");
    expect(sent[0]).toContain("读懂主链路");
    expect(answered).toHaveLength(0);
  });

  it("多题：题面与选项按纯文本渲染（不解释 HTML）", () => {
    const { chat } = mount();
    chat.handleEvent(
      choiceEvent({
        choice_id: "choice-xss",
        wait: false,
        questions: [
          {
            id: "q1",
            question: "<img src=x onerror=alert(1)>",
            options: [{ label: "<b>bold</b>" }, { label: "ok" }],
          },
          { id: "q2", question: "第二题", options: [{ label: "a" }, { label: "b" }] },
        ],
      } as Partial<Extract<ChatEvent, { type: "ChoiceRequested" }>>),
    );
    expect(scope().querySelector("img")).toBeNull();
    expect(scope().querySelector(".choice-q-block b")).toBeNull();
    expect(qBlocks()[0].querySelector(".choice-question")!.textContent).toContain(
      "<img src=x onerror=alert(1)>",
    );
  });

  it("单题形态逐字不变：没有 questions 时仍走原来的单题渲染", () => {
    const { chat } = mount();
    chat.handleEvent(choiceEvent());
    expect(qBlocks(), "单题不该产生多题块").toHaveLength(0);
    expect(scope().querySelector(".choice-card.multi-q")).toBeNull();
    // 弹窗里那张卡片仍是单题结构。选项数 = 2 个模型给的 + 1 个
    // "其他（自定义）"（单题路径的既有行为，多题路径不加这一项）。
    const card = cardInOverlay()!;
    expect(card.querySelectorAll(".choice-opt").length).toBe(3);
    expect(card.querySelector(".choice-question")!.textContent).toBe("鉴权方案选哪个？");
  });

  // ─── ask 附带的学习文档（document_path / document_markdown）──────
  it("附带学习文档：markdown 渲染在问题之后、选项之前，可折叠，选项仍可点", () => {
    const { chat } = mount();
    chat.handleEvent(
      choiceEvent({
        document_path: "docs/jwt-vs-session.md",
        document_markdown: "# 鉴权方案对比\n\n- JWT 无状态\n- Session 有状态",
      }),
    );
    const card = cardInOverlay()!;
    const doc = card.querySelector<HTMLElement>(".choice-doc");
    expect(doc, "应渲染文档块").toBeTruthy();
    expect(doc!.querySelector(".choice-doc-head")!.textContent).toContain("📄");
    expect(doc!.querySelector(".choice-doc-head")!.textContent).toContain(
      "docs/jwt-vs-session.md",
    );
    // markdown 被渲染：# 标题 → heading，列表 → ul/li。
    const body = doc!.querySelector(".choice-doc-body.markdown-body")!;
    expect(body.querySelector("h3")!.textContent).toContain("鉴权方案对比");
    expect(body.querySelectorAll("li")).toHaveLength(2);
    // 位置：问题标题行之后、选项区之前。
    const children = Array.from(card.children);
    const headIdx = children.findIndex((c) => c.classList.contains("choice-head"));
    const docIdx = children.indexOf(doc!);
    const optsIdx = children.findIndex((c) => c.classList.contains("choice-opts"));
    expect(headIdx).toBeGreaterThanOrEqual(0);
    expect(headIdx).toBeLessThan(docIdx);
    expect(docIdx).toBeLessThan(optsIdx);
    // 折叠切换：默认展开，点「收起」body 隐藏、按钮文案变「展开」。
    const toggle = doc!.querySelector<HTMLButtonElement>(".choice-doc-toggle")!;
    expect(toggle.textContent).toBe("收起");
    toggle.click();
    expect(doc!.classList.contains("collapsed")).toBe(true);
    expect(toggle.textContent).toBe("展开");
    toggle.click();
    expect(doc!.classList.contains("collapsed")).toBe(false);
    // 文档不影响作答：选项按钮仍可点、提交可用。
    opts()[0].click();
    expect(submitBtn().disabled).toBe(false);
  });

  it("卡片里的文档链接相对文档所在目录解析（嵌套 md / 图片不 404）", () => {
    const { chat } = mount();
    chat.handleEvent(
      choiceEvent({
        document_path: ".latte/sessions/ui-1/learn/own/draft.md",
        document_markdown: "[第一章](sections/k1.md)\n\n![布局](img/ml-1.svg)",
      }),
    );
    const body = cardInOverlay()!.querySelector(".choice-doc-body")!;
    expect(body.querySelector<HTMLAnchorElement>("a.doc-link")!.dataset.docPath).toBe(
      ".latte/sessions/ui-1/learn/own/sections/k1.md",
    );
    expect(body.querySelector("img")!.getAttribute("src")).toBe(
      `/api/doc-asset?path=${encodeURIComponent(".latte/sessions/ui-1/learn/own/img/ml-1.svg")}`,
    );
  });

  it("多题弹框也渲染附带文档；不带 document_markdown 时零侵入", () => {
    const { chat } = mount();
    chat.handleEvent(
      choiceEvent({
        choice_id: "choice-multi-doc",
        document_path: "docs/guide.md",
        document_markdown: "选前必读",
        questions: [
          { id: "q1", question: "第一题", options: [{ label: "a" }, { label: "b" }] },
          { id: "q2", question: "第二题", options: [{ label: "c" }, { label: "d" }] },
        ],
      } as Partial<Extract<ChatEvent, { type: "ChoiceRequested" }>>),
    );
    const card = cardInOverlay()!;
    const doc = card.querySelector<HTMLElement>(".choice-doc");
    expect(doc, "多题弹框也应有文档块").toBeTruthy();
    expect(doc!.querySelector(".choice-doc-body")!.textContent).toContain("选前必读");
    // 文档在所有题块之前。
    const children = Array.from(card.children);
    expect(children.indexOf(doc!)).toBeLessThan(
      children.findIndex((c) => c.classList.contains("choice-questions")),
    );

    // 缺省/空 document_markdown → 不渲染任何文档块。
    chat.clear();
    chat.handleEvent(choiceEvent({ choice_id: "choice-nodoc" }));
    expect(cardInOverlay()!.querySelector(".choice-doc")).toBeNull();
    chat.clear();
    chat.handleEvent(choiceEvent({ choice_id: "choice-emptydoc", document_markdown: "  " }));
    expect(cardInOverlay()!.querySelector(".choice-doc")).toBeNull();
  });

  it("归档卡片（历史回放）同样渲染文档块", () => {
    const { chat, messagesEl } = mount();
    chat.replayEvents([
      choiceEvent({
        document_path: "docs/old.md",
        document_markdown: "# 旧文档",
      }),
    ]);
    const card = messagesEl.querySelector(".choice-card.answered")!;
    const doc = card.querySelector(".choice-doc");
    expect(doc, "归档卡片也应渲染文档块").toBeTruthy();
    expect(doc!.querySelector(".choice-doc-body h3")!.textContent).toContain("旧文档");
  });

  it("带文档的弹窗加 has-doc 类（默认更宽）；不带文档不加", () => {
    const { chat } = mount();
    chat.handleEvent(
      choiceEvent({ document_markdown: "# 有文档" }),
    );
    expect(
      document.querySelector(".choice-modal")!.classList.contains("has-doc"),
      "带文档的弹窗应加 has-doc",
    ).toBe(true);

    chat.clear();
    chat.handleEvent(choiceEvent({ choice_id: "choice-nodoc-2" }));
    expect(
      document.querySelector(".choice-modal")!.classList.contains("has-doc"),
      "不带文档的弹窗不应加 has-doc",
    ).toBe(false);
  });

});
