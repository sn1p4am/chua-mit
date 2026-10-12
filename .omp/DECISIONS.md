# chua 决策记录（ADR）

> 格式：每条决策一次记录，只增不改；被推翻时追加新条目并标注取代关系。
> 状态：`提议` / `已定` / `待用户拍板` / `已推翻`。

---

## ADR-0001 · 许可与实现路线：MIT + 干净室从零实现 —— 已定（2026-10-12）

**背景**：本产品在功能上与既有的 copyleft 下载器存在重叠。项目发起人此前基于 FluxDown（AGPL-3.0）
做过一版衍生实现（仓库 `chua`，本地 `../chua`），该路线下整体必须 AGPL，无法闭源，也无法做成
纯 MIT 产品。用户决定改为 **完全 MIT**。

**决策**：本仓库（`chua-mit`）以 MIT 许可从零实现，与任何 copyleft 上游**零代码血缘**。
上游仅作为**功能参照**，且严格遵守 `CLEANROOM.md` 的规格侧 / 实现侧分离。

**理由**：

1. 版权只保护表达，不保护思想。重新实现一个功能类似的产品完全合法，前提是实现者未接触对方源码。
2. 干净室流程（规格侧 / 实现侧分离 + 会话日志）把"未接触"从一句声明变成可核查的流程事实。
3. 纯 MIT 让产品可以任意分发、闭源商用、双许可，也不再有 AGPL 第 13 条的网络源码义务。

**代价**：

1. 引擎必须自己写。经普查，功能所需的基础设施（BT/DHT、TLS、HTTP、UI 框架）第三方库全是宽松许可，
   所以真正要自研的是**业务编排层**，不是协议栈本身。
2. 实现会话必须是**从未接触过上游源码的干净会话**，这会降低它的效率（不能参考现成实现）。

**推翻条件**：无。除非用户明确改变产品定位。

---

## ADR-0002 · 前身仓库处理：冻结，只迁移规格不迁移代码 —— 已定（2026-10-12）

**决策**：`../chua`（AGPL 衍生仓库）**冻结并归档**。

- **不做**"把 chua 改成 MIT"——它含 AGPL 代码，改不了；其 git 历史亦是现成的接触证据。
- **不复制任何代码、注释、命名、结构、i18n、fixture、图标**到本仓库。
- **只迁移规格**：把已经想清楚的设计（多源一致性判据、凭据边界、源健康度、失败归因分类）
  用行为语言重述进 `spec/`，由干净会话重新实现。

**理由**：代码是可以被起诉的载体，设计不是。迁移规格能让已有的思考不浪费，同时把表达层留在原地。

---

## ADR-0003 · UI 框架：Zed GPUI（经 `gpui-fast`）—— 有条件选定（2026-10-12）⚠️ **本条的实测复核与两处更正见 ADR-0007**

**结论**：主选 **GPUI / `gpui-fast`**，但**必须先通过下方「验证门槛」的实测**；未通过则退备选。
本条取代此前的「待定（调研中）」占位。

**硬需求**（评审基准，未变）：简约极致的桌面界面；四项速度指标（冷启动 / 空闲占用 / 大列表滚动 /
实时进度刷新）；**中文字体回退与输入法必须可靠**；Windows 优先；宽松许可且 1–2 年内不返工。

**证据**（本地 checkout `gpui-fast-3d7669d275fca649`，许可 Apache-2.0，版权 Zed Industries + Longbridge）：

| 需求 | 证据 | 结论 |
|---|---|---|
| Windows 图形后端 | `crates/gpui_windows/src/directx_devices.rs`：`D3D11CreateDevice`，特性级 `11_1 / 11_0 / **10_1**`；`directx_renderer.rs`：DXGI 合成交换链 + HLSL | ✅ 特性级 10.1 ≈ 2009 年后的显卡即可，覆盖国内老设备 |
| 无 GPU / 远程桌面 | `directx_atlas.rs` 测试路径使用 `D3D_DRIVER_TYPE_WARP`（软件光栅化）；`handle_device_lost` 有设备丢失恢复 | ✅ 有回退路径，不会直接崩 |
| 备用渲染后端 | 独立 crate `gpui_wgpu`（wgsl 着色器，含 `shaders_webgl.wgsl`） | ✅ 需要时可切到 wgpu（DX12 / Vulkan / GL） |
| 中文文本 | `direct_write.rs`：`DirectWriteTextSystem` 实现 `PlatformTextSystem`，用系统 DirectWrite 做字体选择、字形索引/步进/偏移/簇映射与光栅化 | ✅ 直接吃 Windows 原生 CJK 字体回退 |
| **中文输入法** | `events.rs`：处理 `WM_IME_STARTCOMPOSITION` / `WM_IME_COMPOSITION`，调用 `ImmGetContext`、`ImmGetCompositionStringW`（`GCS_RESULTSTR` / `GCS_COMPSTR` / `GCS_CURSORPOS` / `GCS_COMPATTR`）；`handle_ime_position` 把候选窗定位到光标；`update_ime_enabled` 按窗口开关，并显式防串窗（注释指出 IME context 是线程级的） | ✅ **生产级实现，不是占位** |
| 大列表滚动 | `fast/layers/lists.rs`：列表按 **GPU tile** 缓存，用 `tile_hashes` / `dirty_tiles` 只重绘变化的 tile | ✅ 正对「5 万行滚动」 |
| 高频增量刷新 | README 基准（Linux release，每帧 CPU）：「流式行情刷新」5.06ms → **1.35ms**；「列表滚动」5.95ms → **0.79ms**；「窗口无变化」3.31ms → **0.19ms** | ✅ 该场景就是下载器的实时进度列表 |
| **空闲不烧帧** | `app.rs:1903/1914` 只在 `window.invalidator.is_dirty()` 或存在 next-frame 回调时才绘制；`vsync.rs` 的 vsync 线程有明确 idle 概念；`profiler.rs` 有 `animating_at_last_present = next_frame_scheduled && window_active` | ✅ **脏标记驱动**，不是无脑每帧重绘——这是「空闲 CPU < 0.1%」能否成立的关键 |
| 上游可跟进 | `UPSTREAM` 固定 zed commit + `script/check-upstream` + `docs/upstream-sync.md` | ✅ 有脚本化同步流程，不是死分支 |

**风险**（必须正视，不能只看上表）：

1. **成熟度**：README 自述 *"The project remains experimental"*；crates.io 上是 0.1.0；
   checkout 内 `crates/gpui` 版本 0.2.2 且 `publish = false`。用法是改名引用
   （`gpui = { package = "gpui-fast" }`）外加 `[patch.crates-io]`，构建系统有额外复杂度。
2. **两层上游**：Zed → gpui-fast → 我们。任一层破坏性变更都要跟。
3. **不自带控件库**：GPUI 只给原语。按钮 / 输入框 / 对话框 / 表格要另找
   `gpui-component` / `gpui-kit`（Longbridge，0.7.1）——那是第三个依赖，其许可与成熟度
   要一并确认。
4. **保留模式会在特定情况下关闭**：`docs/retained-mode.md` 明确列出窗口刷新（缩放 / 焦点变化）、
   拖拽中、inspector 取色、无障碍激活时**不复用**上一帧。前两项对本产品可接受
   （拖拽只用于排队任务的排序）。
5. **无障碍与保留模式互斥**：M5 不含无障碍，但需要后续方案。

**验证门槛（ADR 完全定稿的前置）**：做一个一次性 spike，**放在本仓库之外的临时目录**
（不产生入库产物，因此不触发 `CLEANROOM.md` 的规格侧/实现侧限制），只回答五个问题：

1. 冷启动到首帧实测（目标 < 300 ms）——这是「极速」里最难的一项。
2. 5 万行虚拟化列表 + 20 行 10 Hz 更新：滚动最长帧时间、单帧 CPU 时间。
3. 窗口打开且无任务时的**空闲 CPU 与内存**（对应 < 0.1% 与 < 80 MB）——
   验证上表最后一行「脏标记驱动」在真实应用里确实生效。
4. 中文：字体回退是否缺字 / 豆腐块；微软拼音或搜狗在输入框内的**候选窗定位**是否正确。
5. 二进制体积，以及在一台干净 Windows 上 `cargo build` 能否跑通（含 `[patch.crates-io]`）。

**任一项不达标即转备选。备选顺序**：

1. **iced**——wgpu + cosmic-text，保留式（Elm 架构），MIT，中文字体走 cosmic-text。
2. **egui**——生态最大、上手最快、MIT/Apache，但**即时模式正是「大列表 + 高频刷新」最吃力的形态**，
   需要自己做缓存规避。
3. **Win32 / WinUI 原生**——速度上限最高，但开发成本与中文输入法细节要自己扛。

**为什么值得冒成熟度的风险**：本产品对 UI 的核心诉求是「大列表 + 高频增量刷新 + 常驻低占用」，
而 `gpui-fast` 存在的唯一理由就是把这三件事做快，并且有可复核的实测数字、有脚本化的上游同步流程、
许可是宽松的 Apache-2.0。换成即时模式框架，这三件事都会变成持续的对抗。


---

## ADR-0004 · 引擎以独立 crate 形式与宿主解耦 —— 提议

**提议**：下载引擎是不依赖任何 UI / FFI 框架的独立 crate，宿主（桌面 UI、CLI、后续可能的移动端）
只通过事件出口与请求入口与之交互。

**理由**：

1. 让引擎可以在无图形环境下用集成测试验证（本地起 range server 跑真实下载），这是 M1 的验收方式。
2. UI 框架尚未定稿（ADR-0003），引擎不应被这个决定绑死。
3. 引擎的依赖树保持最小、全部宽松，便于日后做许可审计。

**待定**：事件出口用 trait 还是 channel；是否需要一个 wire 层（协议 DTO）还是宿主直接链接引擎。
留到 M1 建立 workspace 时定稿。

---

## ADR-0005 · 依赖政策：只允许宽松许可 —— 已定（2026-10-12）

**决策**：依赖白名单 = MIT / Apache-2.0 / BSD-2/3-Clause / ISC / Zlib / Unlicense / BSL-1.0 /
Unicode-3.0 / CC0，以及**未修改的** MPL-2.0。

**黑名单**：AGPL、GPL（任何版本）、LGPL、SSPL、BUSL、Elastic License、Commons Clause，
以及任何带"禁止商用"或"禁止再分发"附加条款的许可。

**执行方式**：

1. 新增依赖必须在 `DECISIONS.md` 追加记录，附许可证据（本地 `Cargo.toml` 的 `license` 字段或上游
   `LICENSE` 文件）。
2. M1 建立 workspace 时引入 `cargo-deny`（MIT/Apache-2.0）做 CI 门禁，配置文件入库。

**证据**：本机 cargo 缓存的 429 个 crate 普查结果——208 个 `MIT OR Apache-2.0`、92 个 `MIT`、
36 个 `Unicode-3.0`、23 个 `Apache-2.0 OR MIT`、21 个 `MIT/Apache-2.0`，唯一 copyleft 是 2 个
MPL-2.0。说明该领域的基础设施生态完全是宽松许可的。

---

## ADR-0006 · 规格侧 / 实现侧分离 —— 已定（2026-10-12）

**决策**：`spec/` 由接触过上游源码的会话编写，`src/` 由从未接触过上游源码的会话编写，两者不得越界。
规格版本号单调递增；实现侧不得自行修改规格，只能提缺口。

**理由与执行细则**：见 `CLEANROOM.md` §3–§5。

**代价**：实现侧效率下降（不能参考现成实现），且规格一旦有缺口就要等规格侧更新。接受这个代价——
它换来的是整个项目可辩护的许可地位。

---

## ADR-0007 · ADR-0003 的实测复核：两处更正、一组实测数据、最终选定 —— 已定（2026-10-12）

**背景**：ADR-0003 把 GPUI 定为「有条件选定」，并以 5 项实测为门槛。本轮完成了实测，
同时用 `research/gpui-evaluation-report.md` 做了交叉核对。**ADR-0003 的证据表里有两处是错的**，在此更正。

### 更正 1（重要）：不存在软件渲染回退

ADR-0003 写「`directx_atlas.rs` 测试路径使用 WARP，有回退」。查证结果：WARP 只出现在该文件的
**`#[cfg(test)]` 路径**里，是单元测试用的。生产路径 `platform.rs:406` 的 `attach_gpu`
在取不到适配器时**直接 panic**，`get_adapter` 以 `unreachable!()` 收尾。

**结论**：在无可用 D3D11 适配器的机器上（老虚拟机、部分远程桌面会话、驱动异常），
程序会**崩溃**而不是降级。这是必须自己补的缺口：启动时做一次 D3D11 预检，失败时给可读的错误窗口。

### 更正 2（重要）：空闲确实不烧帧，但 ADR-0003 给的机制解释不完整

ADR-0003 依据 `app.rs:1903/1914` 判定帧循环是脏标记驱动——方向对，但报告指出还存在一条
每窗口 `DwmFlush` + `RedrawWindow` 的 vsync 线程（`platform.rs:433-495`），且 D3D 设备默认保留，
所以「空闲是否真为零」在实测之前**不能断言**。实测结果见下：**确实是零**。

### 实测数据

本机 i9-13900K / 64 GB / Windows，**release 构建**，`gpui-fast` 0.1.6，
界面为 50,000 行虚拟化列表 + 20 行 10 Hz 更新的中文界面（spike 在仓库外，见 `.omp/TODO.md`）：

| 门槛项 | 预算 | 实测 | 结论 |
|---|---|---|---|
| 冷启动到首帧 | < 300 ms（目标 150） | **236 ms**（无任务）/ 290 ms（含 10 Hz 更新） | ✅ 过线，**未达 150 ms 目标** |
| 空闲 CPU（窗口开着、无任务） | < 0.1% | **0.000%**——25 秒内**一帧都没画**（stdout 只有 `COLD_START_MS`，没有任何 `FRAMES` 行） | ✅ 远超预期 |
| 空闲内存（窗口开着） | < 80 MB | **61 MB** | ✅ |
| 5 万行 + 20 行 10 Hz 更新 | 10 Hz 不重排 | **1.25% 单核**、75 MB、实际帧率 **≈9.5 fps**（严格跟随数据变化，不是 60 fps 空转） | ✅ |
| 5k–50k 行内存 | < 200 MB | **61–75 MB** | ✅ |
| 二进制体积 | < 30 MB | **10.9 MB** | ✅ |
| 中文字体 + 输入法 | 必须可用 | **无法自动测**，需人工在输入框里用微软拼音/搜狗实测 | ⏳ |

**「25 秒零帧」是本次最有价值的实测**：它同时证伪了「vsync 线程会持续重绘」的担忧，
并把「空闲 CPU < 0.1%」从推断变成了观测。注意前提是**窗口可见且无数据变化**；
最小化到托盘是另一条代码路径，M5 必须单独测。

### 遗留风险与缓解（逐条给出对策）

| 风险 | 报告证据 | 对策 |
|---|---|---|
| **生态活跃度过高**（原报告结论为 fail） | `gpui` 最后发布 2025-10-22；`gpui-pre` 39 天发了 10 个版本；`gpui-fast` 4 天从 0.1.0 到 0.1.6；gpui-kit 精确锁 `gpui-pre = "=0.3.8"` 并注明插值范围曾导致编译失败；仓库 2026-09-24 才建，188★ | **按 git rev vendor 进来自己维护**，不跟随上游；CI 检查「与上游的同步年龄」。这消除 churn 风险，代价是自己扛维护 |
| 无适配器时 panic | 更正 1 | 启动做 D3D11 预检，给可读错误窗口 |
| IME 候选窗位置 | issue #64850 仍开着；光标只以单点 + `CFS_CANDIDATEPOS` 传入、没有排除矩形，下方无空间时候选窗压住候选文字；Zed 跟踪器里中文 IME 缺陷历史悠久 | 自己带一个 `CFS_EXCLUDE` 补丁 |
| 托盘不存在 | 全仓搜 tray / `Shell_NotifyIcon` 只有一个误命中 | 引入 `tray-icon`（Apache-2.0 OR MIT） |
| 通知静默失效 | 不调用 `App::set_app_identity` 时 toast 静默 no-op | 启动时必调 |
| 保留模式在特定情况关闭 | 窗口缩放 / 焦点变化、拖拽中、inspector 取色、无障碍激活时不复用上一帧 | 可接受（拖拽仅用于排队任务排序）；无障碍需另行方案 |
| 控件库是第三个 fork | GPUI 核心无控件；gpui-kit 0.7.1 提供 75+ 组件（Apache-2.0） | 先只用核心 + 自绘；确需组件时再单独评审引入 |

### 最终决定

**采用 GPUI / `gpui-fast`，但按「vendor 进来自己维护」的姿态，而不是当作跟随的库。**

实测之后选它的理由更强了：6 项可自动验证的门槛全部通过，其中「空闲零帧」与
「5 万行 + 10 Hz 只占 1.25% 单核」正是本产品最吃紧的两项，而这恰好是不管哪个即时模式框架
都最难做到的。

- **唯一未验证项**：中文输入法与字体回退，需人工实测。
- **未达目标项**：冷启动 236 ms，过 300 ms 门槛但未达 150 ms 目标。慢机器与冷字体缓存下的表现未测；
  **若最差目标机器超过 300 ms，本决定需要重新评估。**

**备选（若上述任一失效）**：iced（wgpu + cosmic-text，MIT）> egui。
注意报告发现 **wgpu 没有 D3D11 后端**——换到 wgpu 系框架会把 GPU 兼容门槛**抬高**而不是降低，
这在目标市场是真实代价。

---

## ADR-0008 · 依赖栈基线（M1 起生效）—— 已定（2026-10-12）

依据 `research/permissive-rust-stack-report.md`（本机 429 个缓存 crate 的全量许可普查 + 源码能力核对）。

### 两条影响架构的决定

1. **TLS 只走 `rustls` + `rustls-platform-verifier`。** `reqwest` 0.13 已把 rustls 作为默认；
   `rustls-platform-verifier` 委托 Windows 系统验证器。因此**不要使用 `webpki-roots`**——
   那是冻结的 Mozilla 证书集，恰恰是在国内/代理环境下会出问题的那种配置。
   这条同时解决了历史遗留的「要不要 native-tls」纠结。
2. **绝不链接 `libav*`。** FFmpeg 默认 LGPL-2.1+（带动态链接与重链接义务），`--enable-gpl` 直接变 GPL，
   会把 MIT 应用拖下水。方案：**内置纯 Rust 转封装**（`ts2mp4` 等，年轻但许可干净），
   把「调用外部 `ffmpeg.exe`」做成**用户可选**的增强——进程边界不构成衍生作品。

### 许可白名单需要扩一条

**`Unicode-3.0`**：经由 `url → idna → icu_*` 不可避免地带进来（36 个 crate）。宽松且 OSI 认可，
但**不在 ADR-0005 的白名单里**——这里显式加上，而不是默默接受。ADR-0005 的白名单据此扩充。

### 关键选型（完整表见报告 §12）

`tokio` / `reqwest` 0.13 / `rustls` 0.23（**不要** pin 0.24.0-dev.1，那是预发布）/
`librqbit` 9（Apache-2.0；磁力、无 tracker 的 DHT、选择性下载、做种、建种全都有，**缺 BEP 19 web seeds**）/
`bendy`（BSD-3-Clause，bencode 规范化，影响 infohash 正确性）/
`suppaftp` 12（注意缓存里的 6.3.0 是旧版本）/ `russh` + `russh-sftp`（Apache-2.0 纯 Rust，优于 `ssh2`）/
`rusqlite` + `refinery` / `backon` / `governor` / `interprocess`（单实例）/ `tray-icon`。

### 明确排除

`slint`（**GPL-3.0-only 或 Slint 商业条款——MIT 产品的陷阱**）、`libproxy` / `pacparser`
（底层 C 库是 LGPL）、`libssh-rs`、`hyper-proxy`（2021 年即停）、`backoff` / `sled` / `lmdb-rkv`（均已停滞）。

### 必须自研的大项（无现成 crate）

多连接分段 HTTP 下载器、多源镜像聚合、Metalink、**整个 eD2K 客户端**（含 ed2khash 的 MD4 树、
AICH、partfile 格式——公开规范清单见报告）、BT web seeds（BEP 19/17）、HLS/DASH 下载与拼装、
PAC 求值、Windows 文件关联与自定义 URI scheme、EWMA 速率与 ETA 模型、全局调度与带宽治理、
文件名净化（安全关键）。

**注意**：报告把 `emulebb-rust`（一个 eMule 对等的 Rust 客户端）标为**连当代码参考都不行**——
eMule 是 GPL-2.0。这与 `CLEANROOM.md` R1/R2 完全一致：**不得阅读**。

