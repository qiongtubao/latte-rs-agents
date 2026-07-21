// 日志面板：查看 ui-sessions 日志文件，方便排查页面卡死等问题。
import { listLogs, LogFile } from "./api";
import { getTransport } from "./transport";

interface UIBinding {
    panelEl: HTMLElement;
    openBtn: HTMLButtonElement;
    closeBtn: HTMLButtonElement;
    refreshBtn: HTMLButtonElement;
    logListEl: HTMLElement;
    logContentEl: HTMLElement;
    statusEl: HTMLElement;
}

export function mountLogPanel(opts: { container: UIBinding }) {
    const { panelEl, openBtn, closeBtn, refreshBtn, logListEl, logContentEl, statusEl } = opts.container;

    openBtn.addEventListener("click", () => {
        panelEl.classList.remove("hidden");
        void refresh();
    });

    closeBtn.addEventListener("click", () => {
        panelEl.classList.add("hidden");
    });

    refreshBtn.addEventListener("click", () => {
        void refresh();
    });

    // 双击日志条目查看内容
    logListEl.addEventListener("dblclick", async (e: Event) => {
        const target = e.target as HTMLElement;
        const item = target.closest(".log-file-item") as HTMLElement | null;
        if (!item || !item.dataset.file) return;
        await loadLogFile(item.dataset.file);
    });

    async function refresh() {
        statusEl.textContent = "加载中…";
        try {
            const files = await listLogs();
            renderFileList(files);
            if (files.length > 0) {
                await loadLogFile(files[0].file);
            } else {
                logContentEl.textContent = "(暂无日志文件)";
                statusEl.textContent = `共 0 个日志文件`;
            }
        } catch (e) {
            statusEl.textContent = `加载失败: ${e}`;
            logContentEl.textContent = `错误: ${e}`;
        }
    }

    function renderFileList(files: LogFile[]) {
        logListEl.innerHTML = "";
        if (files.length === 0) {
            const empty = document.createElement("div");
            empty.className = "log-file-item";
            empty.textContent = "(暂无日志)";
            logListEl.appendChild(empty);
            return;
        }
        for (const f of files) {
            const div = document.createElement("div");
            div.className = "log-file-item";
            div.dataset.file = f.file;
            const sizeStr = f.size > 1024 ? `${(f.size / 1024).toFixed(1)}KB` : `${f.size}B`;
            const date = f.modified > 0 ? new Date(f.modified).toLocaleString("zh-CN") : "";
            div.innerHTML = `
                <span class="log-file-name">${escapeHtml(f.file)}</span>
                <span class="log-file-meta">${sizeStr} · ${date}</span>
            `;
            div.addEventListener("click", () => {
                logListEl.querySelectorAll(".log-file-item").forEach(el => el.classList.remove("active"));
                div.classList.add("active");
            });
            logListEl.appendChild(div);
        }
        const first = logListEl.querySelector(".log-file-item");
        if (first) first.classList.add("active");
    }

    async function loadLogFile(fileName: string, tail: number = 100) {
        statusEl.textContent = `读取 ${fileName}…`;
        try {
            const entries = await getTransport().request("GET", `/api/logs?file=${encodeURIComponent(fileName)}&tail=${tail}`) as LogFile[];
            if (entries.length === 0) {
                logContentEl.textContent = "(文件不存在或为空)";
                statusEl.textContent = `${fileName}: 空`;
                return;
            }
            const entry = entries[0];
            logContentEl.innerHTML = `<div class="log-file-header">${escapeHtml(entry.file)} (${entry.lines.length} 行 / ${formatSize(entry.size)})</div>`;
            const lines = entry.lines;
            if (lines.length === 0) {
                logContentEl.innerHTML += "<div class='log-line'>(空文件)</div>";
            } else {
                for (const line of lines) {
                    const lineDiv = document.createElement("div");
                    lineDiv.className = "log-line";
                    const colonIdx = line.indexOf(": ");
                    if (colonIdx > 0) {
                        const lineNum = line.substring(0, colonIdx);
                        const content = line.substring(colonIdx + 2);
                        const numSpan = document.createElement("span");
                        numSpan.className = "log-line-num";
                        numSpan.textContent = lineNum;
                        lineDiv.appendChild(numSpan);
                        const contentSpan = document.createElement("span");
                        contentSpan.className = "log-line-content";
                        contentSpan.textContent = content;
                        lineDiv.appendChild(contentSpan);
                    } else {
                        lineDiv.textContent = line;
                    }
                    logContentEl.appendChild(lineDiv);
                }
            }
            statusEl.textContent = `${entry.file}: ${entry.lines.length} 行`;
        } catch (e) {
            statusEl.textContent = `读取失败: ${e}`;
            logContentEl.textContent = `错误: ${e}`;
        }
    }

    function formatSize(bytes: number): string {
        if (bytes > 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)}MB`;
        if (bytes > 1024) return `${(bytes / 1024).toFixed(1)}KB`;
        return `${bytes}B`;
    }

    function escapeHtml(text: string): string {
        return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    }
}
