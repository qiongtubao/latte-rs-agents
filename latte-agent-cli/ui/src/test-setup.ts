// Vitest 全局测试 setup。
//
// 引入 @testing-library/jest-dom 以获得 DOM 断言匹配器
// (toBeInTheDocument / toHaveTextContent 等)。在 node 环境的
// 既有测试中引入它是无害的——它只是扩展 expect 的匹配器。
import "@testing-library/jest-dom/vitest";
