# chua

面向中国大陆网络环境的极速多协议下载器。**MIT 许可。**

> **状态：pre-alpha。** 仓库刚刚建立，尚无可用代码。当前工作是写出行为规格（`spec/`），
> 然后由从未接触过 copyleft 上游源码的实现会话从零编码。见 `CLEANROOM.md`。

## 目标

- **快**：冷启动、空闲占用、大列表滚动、实时进度刷新，四项都要快；不做"能跑就行"的界面。
- **多协议**：HTTP/HTTPS、BitTorrent（含磁力）、HLS/DASH、FTP/FTPS、SFTP。
- **面向中国大陆网络**：镜像源聚合、签名 URL 与限流场景的对抗、代理与 PAC、中文排障提示。
- **简约极致**：默认界面只保留必需元素；所有复杂能力都在需要时才出现。

## 许可

MIT，全文见 `LICENSE`。所有依赖必须为宽松许可；本项目**不含**任何 AGPL / GPL / LGPL 代码。

## 文档

| 位置 | 内容 |
|---|---|
| `CLEANROOM.md` | **许可红线与干净室记录** —— 任何会话动手前必读 |
| `spec/` | 行为规格。实现侧的唯一信息来源 |
| `.omp/PLAN.md` | 产品方案与架构 |
| `.omp/ROADMAP.md` | 里程碑与验收标准 |
| `.omp/PROGRESS.md` | 当前进度（唯一事实源） |
| `.omp/TODO.md` | 任务清单 |
| `.omp/DECISIONS.md` | 架构决策记录（ADR） |
| `.omp/PITFALLS.md` | 踩坑记录 |
| `AGENTS.md` | AI 开发契约 |

## 参与

提交前请阅读 `AGENTS.md` 与 `CLEANROOM.md`。**如果你的会话读过 copyleft 上游的源码，你只能写 `spec/`，不能写 `src/`。**
