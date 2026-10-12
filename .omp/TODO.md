# chua 任务清单

> 动手前把要做的事写进来；开始一项改 `[~]`，做完立刻改 `[x]`，不做的删掉并写明理由。
> 一项 TODO 应是一次提交能完成的量。跨里程碑的长期事项写 `ROADMAP.md`。

**图例**：`[ ]` 待做 · `[~]` 进行中 · `[x]` 完成

> ⚠️ **所有 `cargo` 命令前必须先设置 `TMP`/`TEMP` 到工作区内目录**，否则 C 依赖会编译失败。
> 见 `.omp/PITFALLS.md` P-001。

```powershell
$env:TMP = "E:\dsh-workspace\tdmfast\.tmp-build"; $env:TEMP = $env:TMP
```

---

## M0 · 治理与规格

- [x] 建立 MIT 仓库骨架（`LICENSE` / `README.md` / `AGENTS.md` / `.gitignore` / `.gitattributes`）
- [x] `CLEANROOM.md`：红线、规格审查标准、会话日志、逐文件来源表
- [x] `.omp/DECISIONS.md` ADR-0001…0008
- [x] `.omp/PLAN.md`（9 项性能预算）与 `.omp/ROADMAP.md`（M0–M6 验收标准）
- [x] `spec/engine-v1.md` 引擎行为规格 v1.0.0
- [x] `spec/ui-v1.md` 界面规格 v1.0.0（框架无关部分）
- [x] UI 框架调研 → ADR-0003 → **实测复核 ADR-0007**：定稿 GPUI / `gpui-fast`
- [x] 宽松 crate 栈普查 → **ADR-0008 依赖栈基线**
- [x] 两份调研报告入库 `.omp/research/`
- [x] `.omp/PITFALLS.md` P-001：解开本机编译阻塞
- [x] GPUI 5 项门槛实测（spike 在仓库外 `../.gpui-spike`）
  - [x] 冷启动 **236 ms** / 290 ms
  - [x] 空闲 CPU **0.000%**（25 秒零帧）、空闲内存 **61 MB**
  - [x] 5 万行 + 10 Hz 更新：**1.25% 单核**、75 MB、≈9.5 fps
  - [x] 二进制体积 **10.9 MB**
  - [x] 干净构建（debug 41 s / release 59 s）
- [ ] **人工实测中文输入法**（唯一未验证的门槛）
  - [ ] 微软拼音：输入框内候选窗定位正确
  - [ ] 搜狗拼音：同上
  - [ ] Rime / 小狼毫：同上
  - [ ] **重点**：靠近窗口**下边缘**的输入框（IME issue #64850 的确切触发条件）
  - [ ] 100% 与 150% DPI 各测一遍
  - [ ] 中文字体回退无缺字（界面里混排简体、繁体、日文假名、emoji 各看一次）
- [x] 最小 CI（`.github/workflows/ci.yml`）
  - [x] `cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings`
  - [x] 分 crate 跑测试（按 `AGENTS.md` §6，不用 `--workspace`）
  - [x] 依赖许可门禁：`deny.toml` + `cargo-deny`，白名单含 ADR-0008 追加的 `Unicode-3.0`，
        并显式禁止 `slint` / `libproxy` / `pacparser`
  - [x] **clean-room 守卫**：`crates/` 与 `spec/` 不得出现上游名或 copyleft 路径，
        本地已双向验证（当前仓库通过；守卫确实能抓到真实匹配）
- [ ] 创建 GitHub 远端并推送（**需用户提供新令牌**）
- [x] `spec/ui-v1.md` §9 实现附录（ADR-0007 定稿后补充）—— 已随 v1.1.0 完成
- [ ] 清理仓库外临时目录（`.buildtest` 已删；`.chua-verify` 属冻结的旧项目，随其归档一并处理）

## M1 · 实现（干净会话进行中）

- [x] 定稿 ADR-0004（引擎与宿主解耦：事件出口用 channel 不用 trait）
- [~] 按 ADR-0008 建立 crate 划分与依赖清单（`crates/engine` + `crates/cli`）
- [~] M1 集成测试脚手架（本地 range server，不引入额外依赖）
- [x] 以干净会话启动实现（`CLEANROOM.md` §3）
- [ ] 规格侧验收实现交付物：
  - [ ] 自己跑 `cargo build` / `cargo test` / `clippy` / `fmt`，不只看会话的自述
  - [ ] **重点复核**：服务器对 Range 请求返回 `200` 而不是 `206` 时，实现是否拒绝写入
        （`spec/engine-v1.md` §6.3 —— 静默损坏文件的最危险一类错误）
    - [x] 判定逻辑本身**已确认正确**：`engine/ranges.rs` 的 `RangeVerdict` 把
          「200 且请求的就是整个文件」与「200 但请求的是部分区间」分开，
          后者一律拒绝；未知长度模式（§4.3）下允许 200，处理得比规格要求更细
    - [ ] 仍需确认**传输循环确实只走这条路径**（模块文档声称 "the transfer loop has no
          path that bypasses it"，但循环尚未落地，需要看代码而不是看注释）
  - [ ] 复核 `done_len` 只统计已持久化字节（§7.3），而非已接收字节
  - [ ] **复核「重启后 `Downloading` 必须回到 `Queued`」这条规则在哪里落实**（§7.3 第 2 条）。
        `store.rs` 只负责存取，该规则应由启动路径应用；确认它确实存在且有测试，
        而不是仅仅"读回来是什么状态就是什么状态"
  - [ ] 复核引擎内不含任何面向用户的文案（§8.4）
  - [ ] 复核命令注入与路径穿越：文件名净化对 `..`、`CON`、绝对路径的处理（§4.2）
  - [x] ~~复核单任务限速~~ —— **已确认支持**：`throttle.rs::effective_limit(global, per_task)`
        取 `min`，`pace()` 同时接全局与任务两个限速器，与 §9.1「两者同时生效时取更严格者」一致
  - [ ] 填入 `CLEANROOM.md` §8 的逐文件来源表

## 已知的必须在 M5 前置处理项（来自 ADR-0007 风险表）

- [ ] D3D11 启动预检（否则无适配器机器直接 panic）
- [ ] 携带 `CFS_EXCLUDE` 候选窗补丁
- [ ] 引入 `tray-icon` 并调用 `App::set_app_identity`
- [ ] 按 git rev vendor `gpui-fast`，CI 检查与上游的同步年龄

---

## 待规格侧回答（实现侧发现缺口时记这里）

_（空）_
