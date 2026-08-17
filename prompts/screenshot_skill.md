# Screenshot Skill — 使用 Playwright 截取 UI 截图并分析

## 概述

使用 Playwright/Chromium 截取页面截图。

### 基本用法

```bash
# 截取 localhost:4567 的页面
chromium-browser --headless --disable-gpu --no-sandbox \
  --screenshot=/tmp/latte-shot.png \
  --window-size=1920,1080 \
  http://localhost:4567/ 2>/dev/null

# 读取截图内容（base64 编码）
echo "data:image/png;base64,$(base64 -w0 /tmp/latte-shot.png)"
```

### 进阶用法：Playwright 脚本

如果安装了 Playwright，可以用完整脚本控制：

```javascript
// screenshot.mjs
import { chromium } from 'playwright';
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1920, height: 1080 } });
await page.goto('http://localhost:4567/');
await page.screenshot({ path: '/tmp/latte-shot.png', fullPage: true });
await browser.close();
```

### 发送图片给模型（Vision API）

如果模型支持 vision（如 Claude Sonnet 4、GPT-4o），截图返回的 base64 data URI 可以直接作为图片消息发送给模型：

```
data:image/png;base64,iVBORw0KGgo...
```

### 使用场景

1. **UI 回归测试**：修改前端后截图，对比前后变化
2. **布局分析**：截图后让模型分析组件布局、间距、颜色
3. **状态验证**：在不同操作后截图，验证 UI 状态变化
4. **跨浏览器测试**：在不同 viewport 下截图验证响应式

### 工作流示例

```
1. 运行 `cd /home/dong/Documents/latte/latte-rs-agents/latte-agent-cli/ui && node screenshot.mjs`
2. 运行 `base64 -w0 /tmp/latte-shot.png` → 获取 data URI
3. 将 data URI 作为图片消息发给 vision 模型分析
```
