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
    - [x] **传输循环确实走这条路径**（已读代码确认）：`transfer.rs:263` 调用
          `validate_range_response`，`:264` 用 `verdict_failure` 转成失败；
          全文件**只有一个写入点**（`:396` 的 `write_all`），且它接收的是已校验过的偏移，
          模块文档声称的「no second path to the file」与代码一致
    - [ ] 端到端跑一次：起一个对 Range 请求回 `200` + 整份文件的本地服务器，
          断言引擎报错而**不是**写出一个损坏文件（等 CLI 落地后做）
  - [ ] 复核 `done_len` 只统计已持久化字节（§7.3），而非已接收字节
  - [x] ~~复核「重启后 `Downloading` 必须回到 `Queued`」这条规则在哪里落实~~ —— **已确认**：
        `engine/mod.rs::Actor::restore()`（`:371`）显式应用 §7.3(2)，把 `Downloading` 归一化为
        `Queued`，`Created` 也一并重新排队，并且有一处断言消息
        `restore skipped an illegal normalisation` 自我校验
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

- **§3.2 的 `→ Failed` 行没有列出 `Paused` 作为来源。** 原文写的是「`{Probing,Queued,Downloading} → Failed`」。
  实现按字面读：`Paused → Failed` **不是**合法迁移（`Paused` 时没有 worker 在跑，也就没有东西能报致命错误）。
  如果规格侧希望「暂停中的任务也能因外部原因进 Failed」，请补一行；否则这条按现状即为正确读法。
  代码位置：`crates/engine/src/state.rs` 的 `can_transition_to`。

- **§9.1 的限速在当前实现中尚未成立。** 见 `.omp/PITFALLS.md` 的 **P-004**：
  `throttle.rs::reserve` 的配速算术有缺陷，限速任务会跑得比上限快，
  因此 5 个配速用例被标为 `#[ignore]` 而不是删掉或放宽。
  **限速未实现**，`set_global_rate_limit` / 每任务 `--limit` 目前不生效。
  速率**测量**（`RateWindow`）与限速的**接线**（`pace`、`effective_limit`、运行中改速率）均已通过测试。

> **以上为 M1 集成测试与 CLI 会话（`implement-m1-integration-and-cli`）之前的状态。**
> 该会话已把配速重写为**单一时间线**（`Pacer`，一个 `reserved_until` 原子 + 每次预留一次
> `compare_exchange`），5 个配速用例去掉 `#[ignore]` 后全部通过；限速经端到端实测生效
> （4 MiB @ 2 MiB/s 用时约 2.05 s，此前 1.04 s 即 2 倍超发）。以下为本轮新发现的规格缺口。

- **§3.2 的迁移表没有 `{Completed,Failed} → Removed`。** 表里只有「非终态 → Removed」，
  照字面读则**已完成/已失败的任务永远无法删除**——而 `spec/cli-v1.md` §2 要求实现 `remove`，
  且用户要清理的恰恰是这些任务。实现按「§3.1 的『终态 MUST NOT 自动离开』约束的是自动迁移，
  宿主显式指令不算」处理，补了 `{Completed,Failed} → Removed` 两条边（与表中已有的
  `Failed → Probing` 显式重试同形）。代码位置：`crates/engine/src/state.rs::can_transition_to`。
  请确认这两条边是否应写进 §3.2。

- **§4.3 未说明未知长度模式如何判定「传完了」，也未说明 `total_len` 之后取什么值。**
  实现采用：关闭连接即是表示结束（收到 EOF 即该段完成），并把实际收到的字节数作为
  `total_len` 记入任务（属于 §4.3 允许的「来源在中途给出了长度」）。此前实现把 EOF 一律
  当作「响应被截断」，未知长度模式**永远无法完成**。请规格侧确认这一读法。

- **§9.1 不变式 2（SHOULD ≤ N/rate + 一个分块）与不变式 5（改速率不追溯）会冲突。**
  不改速率时两者相容；运行中把速率调高时，「不追溯」要求已经发出的预留区间保持不变，
  于是新预留会排在按**旧**速率记下的欠账之后（过慢），而「不超发」不允许反过来缩短欠账。
  实现按 MUST 优先：不动已发出的区间，只改之后预留的单价，并把过慢上界限定在
  「每条传输通道一个分块」的量级。代码位置：`crates/engine/src/engine/throttle.rs::set_rate`。

- **§7.3(1)/§3.2 没写「谁把恢复出来的任务重新跑起来」。** `EngineConfig::auto_resume`
  字段存在但此前未被任何代码读取：`restore()` 把非终态任务归一化到 `Queued` 之后就停住了，
  而 `Queued → Queued` 不是合法迁移，宿主没有任何公开接口能让它重新开始。
  实现为：`restore()` 在 `auto_resume` 为真时对 `Queued`/`Probing` 任务启动 worker；
  由宿主按命令决定（只读命令不启动传输，`resume` 才启动）。另把 `Created` 的归一化目标
  从 `Queued` 改为 `Probing`——`Created → Queued` 不在迁移表里，原实现会**静默丢弃**
  在探测期间被中断的任务。代码位置：`crates/engine/src/engine/mod.rs::restore`。

- **`spec/engine-v1.md` §6.1 的「单段 MUST NOT 小于一个下限」与对齐规则相冲。**
  默认策略下 8 MiB 文件会切出 3 个 1 字节段（对齐到 4 MiB 粒度后落回上一条边界），
  即段大小低于 1 MiB 下限。实现改为：候选边界若会产生低于下限的段（含末尾段）就丢弃，
  其字节并入前一段。请确认该读法，并考虑 `granularity` 与 `min_segment_bytes` 是否应
  在配置校验里约束相互关系。

- **`spec/cli-v1.md` §2 的 `--detach` 在「一次调用即退出的宿主」里没有意义。**
  「登记任务后立即返回」之后，本进程已退出，没有任何东西在驱动传输。当前实现照字面做
  （登记即返回 0），并在输出里提示需要再执行 `resume --all` 才能继续。
  如果规格希望 `--detach` 代表「后台继续下载」，需要指定一种常驻机制（M5 的常驻宿主天然满足）。

- **`spec/cli-v1.md` §4 与验收套件对输出的编码要求不一致。** `spec/acceptance/run.py`
  用 `subprocess.run(text=True)` 捕获子进程输出，按**平台本地编码**解码（本机为 GBK）；
  而 `spec/engine-v1.md` §8.4 要求宿主给出**中文**说明。CLI 若输出 UTF-8 中文，
  套件会在 `UnicodeDecodeError` 上直接崩溃（实测如此，非推测）。
  实现选择：CLI 的输出**全部为 ASCII 英文**，中文文案留给 M5 的 GUI 宿主。
  若规格希望 CLI 也输出中文，需要同时指定输出编码与套件的解码方式。

- **`spec/cli-v1.md` 没有给「引擎拒绝状态迁移」留退出码。** 例如 `pause` 一个已完成任务、
  或 `remove` 一个已被删除的任务。实现的读法是：这属于「任务失败」→ 退出码 2
  （用法错误 1 留给参数本身不合法的情况）。请确认，或补一个码。

- **`spec/cli-v1.md` 未规定多个宿主进程共享同一状态目录时的语义。**
  本 CLI 每次调用都是一个独立进程：`list`/`status` 可以在下载中调用（实测 12/12 次成功、
  无 SQLite 锁冲突），但 `pause`/`remove` 由另一个进程发出时，**无法打断**正在传输的那个进程，
  且其后一条状态上报会覆盖掉刚写入的 `Paused`。ADR-0004 的单 actor 模型隐含「一个状态目录
  一个宿主进程」；验收套件不覆盖这一情形。建议在规格里写明该约束（M5 常驻宿主天然满足）。

- **供规格侧参考（不是缺口）：验收套件的 `resume-after-kill` 证据比自己以为的弱。**
  §6.4 要求预分配目标文件，因此 `run.py` 等 `dest.stat().st_size > PAYLOAD_SIZE // 8` 时，
  文件**一开始就是全尺寸**，`kill` 几乎立刻发生，`partial` 打印出来等于全尺寸。
  该项通过说明「重启后最终字节正确」，但并不能证明「已落盘字节没有被重下」。
  真正的续传证据在 `crates/engine/tests/http_end_to_end.rs` 的
  `an_interrupted_download_resumes_and_the_final_bytes_are_correct`：它按服务端实际发出的
  字节数断言续传没有重下整份文件。若要让套件自己也具备这个力度，需要把判据从
  「文件大小」换成「服务端已发送字节数」（`/_requests` 目前只报请求数，不报字节数）。
