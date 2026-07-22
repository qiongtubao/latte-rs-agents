import { describe, it, expect } from "vitest";

import type { ChatEvent } from "../api";
import {
  createStageStore,
  replayStageHistory,
  selectStageById,
  selectStageLog,
  selectTopLevelStages,
  stageStore,
} from "./stageStore";

// 固定时钟，保证 TraceRecord.timestampMs 可预期。
const fixedNow = () => 1000;

describe("createStageStore — applyEvent", () => {
  it("folds a single event into a top-level stage", () => {
    const store = createStageStore(fixedNow);
    store.getState().applyEvent({
      type: "RoleStarted",
      role_id: "manager",
      detail: "",
    });

    const state = store.getState();
    expect(state.order).toHaveLength(1);
    const stage = state.byId[state.order[0]];
    expect(stage.roleId).toBe("manager");
    expect(stage.status).toBe("in_progress");
  });

  it("threads reducer context across successive applyEvent calls", () => {
    const store = createStageStore(fixedNow);
    const events: ChatEvent[] = [
      { type: "RoleStarted", role_id: "manager", detail: "" },
      { type: "Status", message: "working" },
      { type: "RoleTurn", role_id: "manager", content: "answer", is_complete: true },
    ];
    for (const e of events) store.getState().applyEvent(e);

    const state = store.getState();
    // 只创建了一个 Stage（ctx 委派栈被正确串联，未把后续事件当成新 Stage）。
    expect(state.order).toHaveLength(1);
    const stage = state.byId[state.order[0]];
    expect(stage.status).toBe("complete");
    expect(stage.content).toBe("answer");
    // Status 事件被记入 Event_Trace。
    expect(stage.log.eventTrace.some((r) => r.kind === "Status")).toBe(true);
  });

  it("builds a nested subagent stage tree via the delegation stack", () => {
    const store = createStageStore(fixedNow);
    const events: ChatEvent[] = [
      { type: "RoleStarted", role_id: "manager", detail: "" },
      {
        type: "DelegateStarted",
        from_role: "manager",
        to_role: "programmer",
        task: "impl",
        sub_id: "s1",
      },
      { type: "Status", message: "child working" },
    ];
    for (const e of events) store.getState().applyEvent(e);

    const state = store.getState();
    expect(state.order).toHaveLength(1);
    const parent = state.byId[state.order[0]];
    expect(parent.childIds).toHaveLength(1);
    const child = state.byId[parent.childIds[0]];
    expect(child.parentId).toBe(parent.id);
    expect(child.depth).toBe(1);
    // 子事件只落在子 Stage 的日志里（隔离）。
    expect(child.log.eventTrace.some((r) => r.kind === "Status")).toBe(true);
    expect(parent.log.eventTrace.some((r) => r.kind === "Status")).toBe(false);
  });
});

describe("createStageStore — seed", () => {
  it("folds an event array to initialize state and resets context", () => {
    const store = createStageStore(fixedNow);
    store.getState().seed([
      { type: "RoleStarted", role_id: "manager", detail: "" },
      { type: "RoleTurn", role_id: "manager", content: "hi", is_complete: true },
    ]);

    let state = store.getState();
    expect(state.order).toHaveLength(1);
    expect(state.byId[state.order[0]].status).toBe("complete");
    expect(state.activeLogStageId).toBeNull();

    // 重放结束后 ctx 栈为空：随后的顶层事件应新建第二个 Stage。
    store.getState().applyEvent({ type: "RoleStarted", role_id: "programmer", detail: "" });
    state = store.getState();
    expect(state.order).toHaveLength(2);
  });

  it("replaces the visible stage list when restoring another session", () => {
    // 先放入旧 session 的可见内容，模拟用户从一个会话切到另一个会话。
    stageStore.getState().applyEvent({ type: "RoleStarted", role_id: "stale", detail: "" });

    replayStageHistory([
      { type: "RoleStarted", role_id: "manager", detail: "calling LLM" },
      { type: "RoleTurn", role_id: "manager", content: "restored answer", is_complete: true },
    ]);

    const state = stageStore.getState();
    expect(state.order).toHaveLength(1);
    const restored = state.byId[state.order[0]];
    expect(restored.roleId).toBe("manager");
    expect(restored.content).toBe("restored answer");

    // 共享单例必须复位，避免影响同进程中的其他测试。
    stageStore.getState().seed([]);
  });

  it("clears stale stages and log selection when restored history is empty", () => {
    // 空 session 也必须替换可见状态，不能残留上一个 session 的内容。
    stageStore.getState().applyEvent({ type: "RoleStarted", role_id: "stale", detail: "" });
    const staleId = stageStore.getState().order[0];
    stageStore.getState().openLog(staleId);

    replayStageHistory([]);

    const state = stageStore.getState();
    expect(state.order).toEqual([]);
    expect(state.byId).toEqual({});
    expect(state.activeLogStageId).toBeNull();
  });
});

describe("createStageStore — disclosure & log actions", () => {
  it("toggleExpanded flips the stage's expanded flag", () => {
    const store = createStageStore(fixedNow);
    store.getState().applyEvent({ type: "RoleStarted", role_id: "manager", detail: "" });
    const id = store.getState().order[0];

    expect(store.getState().byId[id].expanded).toBe(false);
    store.getState().toggleExpanded(id);
    expect(store.getState().byId[id].expanded).toBe(true);
    store.getState().toggleExpanded(id);
    expect(store.getState().byId[id].expanded).toBe(false);
  });

  it("toggleExpanded on a missing id is a no-op", () => {
    const store = createStageStore(fixedNow);
    store.getState().toggleExpanded("nope");
    expect(store.getState().byId).toEqual({});
  });

  it("openLog sets and closeLog clears activeLogStageId", () => {
    const store = createStageStore(fixedNow);
    store.getState().applyEvent({ type: "RoleStarted", role_id: "manager", detail: "" });
    const id = store.getState().order[0];

    store.getState().openLog(id);
    expect(store.getState().activeLogStageId).toBe(id);
    store.getState().closeLog();
    expect(store.getState().activeLogStageId).toBeNull();
  });
});

describe("selectors", () => {
  it("selectTopLevelStages returns stages in order", () => {
    const store = createStageStore(fixedNow);
    store.getState().applyEvent({ type: "RoleStarted", role_id: "manager", detail: "" });
    store.getState().applyEvent({ type: "RoleTurn", role_id: "manager", content: "", is_complete: true });
    store.getState().applyEvent({ type: "RoleStarted", role_id: "programmer", detail: "" });

    const tops = selectTopLevelStages(store.getState());
    expect(tops.map((s) => s.roleId)).toEqual(["manager", "programmer"]);
  });

  it("selectStageById / selectStageLog address a stage by id", () => {
    const store = createStageStore(fixedNow);
    store.getState().applyEvent({ type: "RoleStarted", role_id: "manager", detail: "" });
    const id = store.getState().order[0];

    expect(selectStageById(id)(store.getState())?.roleId).toBe("manager");
    expect(selectStageLog(id)(store.getState())).toBeDefined();
    expect(selectStageById("missing")(store.getState())).toBeUndefined();
    expect(selectStageLog("missing")(store.getState())).toBeUndefined();
  });
});
