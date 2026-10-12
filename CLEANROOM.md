# 干净室记录（Clean Room Record）

本文件是本项目许可合规的核心证据，**只增不改**。任何参与本仓库的会话（人类或 AI）在动手前必须先读它。

---

## 1. 为什么需要它

版权法保护的是**表达**，不保护**思想**。

| 不受保护（可自由使用） | 受保护（不得复制） |
|---|---|
| 算法、协议、文件格式、功能需求 | 源码正文、注释、标识符命名 |
| 架构模式、性能目标、行为规格 | 具体的类型/函数/模块结构设计 |
| 公开标准与事实 | 错误文案、i18n 词条、测试 fixture |
| 通用工程知识 | 图标、图片、文档正文、数据编排 |

所以"做一个功能类似的下载器"完全合法。但侵权判定走的是这条路：原告证明 **① 接触（access）+ ② 实质性相似** → 举证责任转移 → **由你证明独立创作**。

而"我读过源码但重写了"几乎无法证明独立创作。干净室流程的作用不是让代码"看起来不一样"，而是让**写代码的人从未接触过对方源码**这件事在流程上成立、并且可核查。

## 2. 上游关系（事实陈述）

本产品在功能上与 **FluxDown**（`https://github.com/zerx-lab/FluxDown`，AGPL-3.0）等既有的 copyleft
下载器存在重叠，并且项目发起人曾fork 并修改过该上游。这一接触事实无法否认，也不打算隐瞒——
它正是本文件存在的原因。

本仓库与该上游**不存在任何代码血缘**：没有 fork 关系、没有 git 历史继承、没有复制任何源码或素材。
功能重叠本身不构成侵权；本流程用来保证表达层不被复制。

## 3. 两类参与方

| 角色 | 谁 | 允许看的 | 产出 |
|---|---|---|---|
| **规格侧**（tainted） | 曾接触过 copyleft 上游源码的会话 | 上游源码、公开协议文档、运行中的产品行为 | `spec/` 下的行为规格 |
| **实现侧**（clean） | **从未接触过上游源码**的会话 | 只允许 `spec/`、公开协议标准、本仓库 | `src/` 下的实现代码 |

**规格侧不得写实现代码；实现侧不得读上游源码。** 这是整个流程的全部要点。

## 4. 硬规则

- **R1** 本仓库不得出现任何来自上游 copyleft 项目的源码、注释、标识符、文件结构、错误文案、
  i18n 词条、测试 fixture、图标或文档正文。
- **R2** 禁止在本仓库内对上游 copyleft 仓库执行 `git clone` / `git fetch` / `git remote add` /
  `git subtree`。本仓库的 git 历史必须从零开始。
- **R3** 实现会话的工作目录必须是本仓库根。禁止把工作目录指向任何 copyleft 源码树，禁止跨目录
  检索这些源码树。
- **R4** 禁止从 copyleft 项目复制任何素材（图标、字体、图片、翻译文件）。
- **R5** 依赖只允许宽松许可：MIT / Apache-2.0 / BSD / ISC / Zlib / Unlicense / BSL-1.0 /
  Unicode-3.0，以及**未修改的** MPL-2.0。新增依赖必须在 `.omp/DECISIONS.md` 记录并附许可证据。
- **R6** 本仓库不得包含任何 AGPL / GPL / LGPL 代码。以"独立进程调用"为理由引入也不接受——
  本项目要的是完全 MIT。
- **R7** 规格必须停在思想层（见 §5）。
- **R8** 每次实现会话结束，在 §7 追加一行记录。

### 机械执行

R1–R4 不是只靠自觉。`.github/workflows/ci.yml` 的 **`cleanroom` job** 在每次推送时检查
`crates/` 与 `spec/`：

- 不得出现上游项目名或其组织名（不区分大小写）
- 不得出现指向 copyleft 目录树的本地路径

只检查 `crates/` 与 `spec/`——`CLEANROOM.md` 与 `.omp/` 是**来源记录**，必须能点名，
否则本文件就成了自相矛盾的文档。规格也必须干净：一份滑向"描述上游怎么做的"的规格，
恰恰是这个检查要拦下的失败形态。

本地等价命令（Git 自带 grep）：

```bash
grep -rniE 'fluxdown|zerx[_-]?lab' crates/ spec/
grep -rnE 'tdmfast[\\/]chua|\.chua-verify' crates/ spec/
```

## 5. 规格审查标准

写规格时逐条自查：

**可以写** —— 描述行为与约束：

> 用「总长度 + 强验证器」判定两个候选源是否为同一文件；只有长度而验证器缺失时不得合并计数。

**需谨慎** —— 描述输入输出的语义，但不落成对方的结构：

> 一致性判据的输入，是每个候选源各自声明的总长度与验证器值。

**不可以写** —— 任何逐行可对应的结构：

> 定义 `SourceEvidence { total_len: Option<u64>, etag, last_modified }` 并实现
> `evaluate()` 返回 `GuardDecision`。

判据很简单：**如果把规格里的名字换掉之后，它和对方的某个文件仍然能一一对应，那就已经越界了。**

## 6. 信息来源白名单

| 来源 | 规格侧 | 实现侧 |
|---|---|---|
| 公开协议标准（RFC、BT BEP、HLS/DASH 规范、eD2K 公开文档） | ✅ | ✅ 首选 |
| 对本产品的外部行为观察（抓包、日志、文件输出、界面交互） | ✅ | ✅ |
| 通用工程知识、公开博客与第三方文档 | ✅ | ✅ |
| 本仓库的 `spec/` | ✅ | ✅ |
| 上游 copyleft 源码 | ⚠️ 仅可用于理解行为，不得转述其表达 | ❌ 禁止 |
| 上游 README / 文档正文 / i18n / 图标 | ❌ | ❌ |

## 7. 会话日志

每次完成一次会话追加一行。**"读过的上游源码"一栏必须如实填写。**

| 日期 | 会话标识 | 侧 | 读过上游源码 | 输入 | 产出 |
|---|---|---|---|---|---|
| 2026-10-12 | 初始建仓会话 | 规格侧 | 是 | 用户需求 | 仓库骨架、`LICENSE`、`README.md`、`AGENTS.md`、本文件、`.omp/` 文档 |
| 2026-10-12 | 规格会话（续） | 规格侧 | 是 | 用户「改走 MIT 路线」的决定 | `spec/engine-v1.md`、`spec/ui-v1.md`、ADR-0003 / 0007 / 0008、`PITFALLS.md` |
| 2026-10-12 | 调研会话 A（子代理） | **干净侧** | **否** —— 仅读 `gpui-fast` 的本地 checkout 与公开网页；明确禁止读 `chua/` | UI 框架评估任务书 | `.omp/research/gpui-evaluation-report.md` |
| 2026-10-12 | 调研会话 B（子代理） | **干净侧** | **否** —— 仅读本机 cargo 缓存、`rqbit` checkout、crates.io 注册表 API 与公开网页；明确禁止读 `chua/` | 宽松 crate 栈普查任务书 | `.omp/research/permissive-rust-stack-report.md` |
| 2026-10-12 | spike 会话 | 规格侧 | 是 | ADR-0003 的 5 项门槛 | 仓库外 `../.gpui-spike`，**无入库产物**（故不受本文件 §3 的分工约束） |

> **两个调研子代理的输入侧是干净的**：它们没有读过上游 copyleft 源码，产出的报告因此可以直接入库。
> 但这不改变 §3 的分工——**`src/` 仍然只能由干净会话编写**。报告是调研结论，不是实现。

## 8. 逐文件来源表

**M1 实现会话标识**：`implement-m1-engine-slice`（独立子代理，输入仅为 `spec/`、`AGENTS.md`、
`CLEANROOM.md`、`.omp/DECISIONS.md`、`.omp/PLAN.md`、`.omp/ROADMAP.md`、`.omp/PITFALLS.md`；
任务书中明确禁止读写任何 copyleft 目录）。**依据的规格版本：`engine-v1.md` v1.0.0。**

| 文件 | 实现会话 | 依据规格 | 接触过上游源码 |
|---|---|---|---|
| `Cargo.toml`（workspace）、`rustfmt.toml` | 同上 | — | **否** |
| `crates/engine/Cargo.toml` | 同上 | ADR-0005、ADR-0008 | **否** |
| `crates/engine/src/lib.rs` | 同上 | §1、§10、ADR-0004 | **否** |
| `crates/engine/src/state.rs` | 同上 | §3（9 状态与合法迁移表） | **否** |
| `crates/engine/src/failure.rs` | 同上 | §8.2（12 类）、§8.3（退避） | **否** |
| `crates/engine/src/http.rs` | 同上 | §4（探测）、§6.3（区间校验）、§8 | **否** |
| `crates/engine/src/filename.rs` | 同上 | §4.2（推导与净化） | **否** |
| `crates/engine/src/config.rs` | 同上 | §6.1、§11 | **否** |
| `crates/engine/src/model.rs` | 同上 | §2（不变式 I1）、§7.3（durable 语义） | **否** |
| `crates/engine/src/events.rs` | 同上 | §9.4、§10、ADR-0004 | **否** |
| `crates/engine/src/util/{mod,percent}.rs` | 同上 | §4.2 | **否** |

**尚未落地**（`lib.rs` 已声明但文件未创建）：`store.rs`（§7）、下载主循环与引擎句柄（§6、§9、§10）、
`crates/cli/**`。这些文件创建后由**实现会话自行追加**到本表。

**说明**：本表只列真实存在的文件。M1 交付时由规格侧逐项复核（复核清单见 `.omp/TODO.md`），
但**表格的填写权属实现侧**——规格侧代填会让这份记录失去作为来源证据的意义。

### 8.1 追加（同一实现会话，续）

**依据的规格版本补充**：`engine-v1.md` **v1.0.1**（§4.2 澄清：含路径的候选名取末段而非拼接）。
实现侧在 v1.0.1 发布后按其改写了 `filename.rs` 的净化顺序，并同步修正了原先编码了拼接读法的两个用例。

| 文件 | 实现会话 | 依据规格 | 接触过上游源码 |
|---|---|---|---|
| `crates/engine/src/store.rs` | `implement-m1-engine-slice` | §7.1、§7.2（原子落盘）、§7.3 | **否** |
| `crates/engine/src/engine/mod.rs` | 同上 | §3.2、§7.3(2)、§9.4、§10、ADR-0004 | **否** |
| `crates/engine/src/engine/persist.rs` | 同上 | §7.2（单一写入者） | **否** |
| `crates/engine/src/engine/job.rs` | 同上 | §4、§6、§7.3、§8.3 | **否** |
| `crates/engine/src/engine/transfer.rs` | 同上 | §6.3（区间校验）、§6.4（按偏移写入） | **否** |
| `crates/engine/src/engine/ranges.rs` | 同上 | §6.3（**最危险的一类错误**的专门分支） | **否** |
| `crates/engine/src/engine/throttle.rs` | 同上 | §9.1、§9.2（**配速部分未实现**，见 `.omp/PITFALLS.md` P-004） | **否** |
| `crates/engine/DEPENDENCIES.md` | 同上 | ADR-0005、ADR-0008 | **否** |

`crates/cli/**` 与本地 range server **未创建**，原因与本轮已知缺陷一并记在 `.omp/TODO.md`。

### 8.2 追加：会话日志（§7 续行）

| 日期 | 会话标识 | 侧 | 读过上游源码 | 输入 | 产出 |
|---|---|---|---|---|---|
| 2026-10-12 | `implement-m1-engine-slice`（子代理） | **干净侧** | **否** —— 只读了 `spec/engine-v1.md`、`spec/cli-v1.md`、`CLEANROOM.md`、`AGENTS.md`、`.omp/` 下的文档，以及公开的 crate 源码（`reqwest` 的本地 registry 副本）；任务书明确禁止读写 `chua/`、`app/`、`tamper-test/`、`.chua-verify` | M1 slice 1 任务书 | `Cargo.toml`、`rustfmt.toml`、`crates/engine/**`（16 个源文件，lib 129 测试通过 / 5 忽略） |
| 2026-10-12 | `implement-m1-integration-and-cli`（子代理） | **干净侧** | **否** —— 输入为 `spec/engine-v1.md`（v1.0.3）、`spec/cli-v1.md`、`CLEANROOM.md`、`AGENTS.md`、`.omp/PITFALLS.md`、`.omp/PROGRESS.md` 与既有 `crates/` 源码；任务书明确禁止阅读任何 copyleft 下载器的源码，也不得为了照抄而上网搜索同类实现 | M1 收口任务书（集成测试 + 限速修复 + CLI） | `crates/engine/tests/{common/mod.rs,http_end_to_end.rs}`、`crates/cli/**`（5 个源文件）、`engine/{job,transfer,throttle,mod,ranges}.rs` 的修复 |

### 8.3 追加：`crates/cli/**` 与集成测试的逐文件来源

**依据的规格版本**：`engine-v1.md` **v1.0.3**（§9.1 配速不变式）、**v1.0.4**（§6.3 结果导向）、
`cli-v1.md` **v1.0.0**。上述澄清均在实现过程中发布并被采纳。

| 文件 | 实现会话 | 依据规格 | 接触过上游源码 |
|---|---|---|---|
| `crates/engine/tests/common/mod.rs` | `implement-m1-integration-and-cli` | §6、§7、§10（真实 socket 的测试基建） | **否** |
| `crates/engine/tests/http_end_to_end.rs` | 同上 | §4.3、§6.3、§7.3(2)、§9.1、§9.3 | **否** |
| `crates/cli/Cargo.toml` | 同上 | ADR-0005、ADR-0008 | **否** |
| `crates/cli/src/main.rs` | 同上 | `cli-v1.md` §2、§3 | **否** |
| `crates/cli/src/cli.rs` | 同上 | `cli-v1.md` §2（命令与参数） | **否** |
| `crates/cli/src/host.rs` | 同上 | `cli-v1.md` §5（`CHUA_DATA_DIR`）、ADR-0004 | **否** |
| `crates/cli/src/error.rs` | 同上 | `cli-v1.md` §3（退出码） | **否** |
| `crates/cli/src/render.rs` | 同上 | `cli-v1.md` §4（纯 ASCII、非 TTY 无控制字符） | **否** |

**说明**：本表由实现会话追加，规格侧只做了版本与路径的事实核对，未改动内容——
表格的填写权属实现侧，规格侧代填会让这份记录失去作为来源证据的意义。
（§8.1 的说明依然成立；此处两条会话日志与其产出表由对应会话自行登记。）

