# chua 多协议规格

**版本**：1.0.0
**状态**：初始版本（M2）
**读者**：实现侧会话
**约束**：只描述"要什么"。见 [`../CLEANROOM.md`](../CLEANROOM.md) §5。

---

## 0. 范围与前提

M2 要打通：**BitTorrent（磁力与 `.torrent`）、HLS、DASH、FTP/FTPS、SFTP**。

本规格假定 `engine-v1.md` v1.0.5 已实现并稳定。**所有协议 MUST 复用它的状态机、
持久化、事件出口与失败分类**，不得各自长出一套。

依赖选型已在 ADR-0008 定稿，此处不重复；新增依赖仍需 ADR。

---

## 1. 统一任务模型（验收标准 6）

### 1.1 一条硬边界

协议的差异 MUST 止步于**「来源解析」与「传输」**两层。
`engine-v1.md` §3 的九状态机、§7 的持久化、§10 的事件出口对四类协议**完全相同**。

宿主 MUST NOT 需要知道任务是 HTTP 还是 BT 才能渲染列表、暂停、或恢复。

### 1.2 任务标识的形式

`source_label` 是什么协议，由**解析结果**决定，不由宿主声明。可接受的输入形式：

| 输入 | 协议 |
|---|---|
| `http://` / `https://` 且响应是普通资源 | HTTP |
| `http://` / `https://` 且响应是 `.m3u8`（按内容类型与扩展名双重判断） | HLS |
| `http://` / `https://` 且响应是 `.mpd` | DASH |
| `magnet:?xt=urn:btih:...` | BitTorrent |
| `torrent-file://<本地路径>` | BitTorrent（见 §1.3） |
| `ftp://` / `ftps://` | FTP |
| `sftp://` | SFTP |

**MUST NOT 依据文件扩展名单独判定协议**——HLS 与 DASH 的判定必须结合内容类型；
一个 `.txt` 里放着 M3U8 也是合法播放列表。

### 1.3 `.torrent` 的哨兵约定

**HTTP 直链的 `.torrent` 不会自动变成 BT 任务。** 依据 `engine-v1.md` 的模型，
一个 URL 就是一次 HTTP 下载；要变成真 BT 任务，必须先取回字节再以种子内容建任务。

因此：**「磁力」与「种子文件内容」是两个不同的建任务入口**，MUST NOT 混为一谈。
宿主要给用户一个明确的「下载这个种子文件」与「用这个种子下载」的区别。

### 1.4 分段的语义映射

M1 的分段是**字节区间**。四类协议里只有 HTTP / FTP / SFTP 天然是字节区间：

| 协议 | 分段的单位 | 落盘方式 |
|---|---|---|
| HTTP / FTP / SFTP | 字节区间 | 按偏移写入（同 M1） |
| BT | piece（且必须整块校验） | 按 piece 写入；**MUST NOT 把 piece 当字节区间切** |
| HLS / DASH | 分片（文件） | 每个分片先落临时文件，再按序拼装 |

**不变式 I1（区间恰好铺满且不重叠）对后两类协议必须以各自的单位成立**：
BT 是 piece 的集合恰好铺满，HLS/DASH 是分片序列恰好铺满。

**BT 的 piece MUST NOT 被拆**——piece 是校验单位，拆开就无法验证。

---

## 2. 统一元数据探测（验收标准 7）

`engine-v1.md` §4 的探测概念对四类协议统一为三个问题：
**总长度、文件名、能否并行**。

| 协议 | 总长度 | 文件名 | 能否并行 |
|---|---|---|---|
| HTTP | `Content-Length` / `Content-Range` | §4.2 的三级优先 | 以实测的 206 为准（§6.3） |
| FTP | `SIZE` 命令 | 路径末段 | `REST` 命令；**以实测为准** |
| SFTP | `stat` | 路径末段 | 多数实现支持偏移读；**以实测为准** |
| BT | 元数据里的总长 | 元数据里的 `name` | 按 piece 并行 |
| HLS | 各分片长度之和 | 播放列表名派生 | 分片级并行 |
| DASH | 选定 representation 各分片之和 | 同上 | 分片级并行 |

**`total_len` 未知是合法状态**（§4.3），每一类协议都必须能表达它——
HLS 的直播播放列表就是长度未知的典型情形。

**探测 MUST 有总时限**；超时按 §8 分类为可重试失败。

---

## 3. BitTorrent

### 3.1 磁力链接

- MUST 支持**无 tracker、仅靠 DHT** 取得元数据并开始下载。
- MUST 支持裸 40 位十六进制 infohash（即 `magnet:?xt=urn:btih:<hex>` 的各种简写形式）。
- 元数据取得前，任务的 `total_len` 未知、文件名未知——这是 §2 允许的状态，
  **不是错误**。此期间任务 SHOULD 处于 `Probing`，并 MUST 向宿主呈现「正在获取元数据」
  这一可区分的信息（否则用户看到的是一个永远 0% 的任务）。

### 3.2 种子文件与选择性下载

- MUST 支持从种子内容选择**文件子集**下载。
- 选择结果 MUST 持久化，恢复后不重问。
- 未选中的文件 MUST NOT 被写入磁盘，也 MUST NOT 计入 `total_len`。
- 未选中的 piece **MUST 仍然被获取**（一个 piece 可能跨越已选与未选文件）——
  但其中的未选部分 SHOULD NOT 落盘。这是 BT 的固有复杂度，实现 MUST 显式处理，
  MUST NOT 假装 piece 与文件一一对应。

### 3.3 做种

- 默认**不做种**（下载完即停）。做种是显式选项。
- 做种时的磁盘活动 MUST 遵守 `engine-v1.md` §12 的空闲静默要求。

### 3.4 私有种子

- 私有种子（元数据标记 `private`）MUST NOT 上报 DHT / PEX。
- 这是一个**正确性要求，不是优化**：违反会使用户被 tracker 封禁。

### 3.5 端口与可达性

- 监听端口 MUST 可配置，且 MUST 在不可用时降级为仅主动连接，MUST NOT 因此失败。
- UPnP / NAT-PMP 打洞失败 MUST NOT 导致任务失败。

---

## 4. HLS

### 4.1 播放列表

- MUST 支持主播放列表（master playlist）与媒体播放列表。
- 主播放列表有多个 variant 时，选择由宿主决定（宿主是唯一知道用户偏好的地方）；
  无宿主选择时 MUST 取码率最高的一个，并把这一默认写进日志。
- MUST 处理 `EXT-X-BYTERANGE`：分片可以是一个文件里的字节区间，
  此时「分片」与「字节区间」两个概念重合，**分段单位回到字节区间**。
- MUST 处理 `EXT-X-MAP` 的初始化分片：它 MUST 排在所有媒体分片之前。
- MUST 处理 `EXT-X-DISCONTINUITY`：它标记时间轴不连续，
  **拼装时 MUST NOT 做任何跨不连续点的处理**。

### 4.2 加密

- MUST 支持 `EXT-X-KEY` 的 `AES-128`：取密钥、解密分片。
- **IV 缺省规则 MUST 实现**：`EXT-X-KEY` 未给 `IV` 时，IV 为**该分片的媒体序号**
  （从 0 开始，按分片在播放列表中的位置计），写成 16 字节大端。
  这是最容易做错的一处——看起来能解密但结果是噪声。
- `SAMPLE-AES` 与 Widevine 等 DRM **明确不做**：没有宽松许可的实现路径。
  遇到它们 MUST 报一个**可区分的失败类**，而不是笼统的「不支持」。

### 4.3 直播与未知长度

- 播放列表里没有 `EXT-X-ENDLIST` 即为直播。
- 直播任务的长度未知（合法状态）；**默认不做直播录制**，
  若要支持，MUST 由用户显式开启并指定停止条件。
- 点播（含 `EXT-X-ENDLIST`）MUST 按有限长度处理，下完即完成。

### 4.4 拼装

- 分片 MUST 按播放列表顺序拼装，MUST NOT 依赖并发完成的顺序。
- 拼装 MUST 是可恢复的：中断后不必重下已完成的分片。

---

## 5. DASH

- MUST 能解析 MPD，并在多个 representation 中选定一个。
  选择由宿主决定；无宿主选择时取带宽最高的一个。
- MUST 支持 `SegmentTemplate`（含 `$Number$` 与 `$Time$` 两种）与 `SegmentList`。
- 分片是字节区间（`SegmentBase` / `indexRange`）时，单位回到字节区间。
- 加密（`ContentProtection`）与 HLS 同样处理：**明确不做 DRM**，报可区分的失败类。

---

## 6. FTP / FTPS 与 SFTP

### 6.1 通用要求

- MUST 支持断点续传（FTP 用 `REST`，SFTP 用偏移读）。
- MUST 支持被动模式（FTP）；主动模式可选。
- **凭据 MUST NOT 出现在日志、事件与错误信息里**。
  这是硬要求：`engine-v1.md` §I4 的凭据边界在这里的形态是「不落盘、不外泄」。
- 用户名/密码为空时的默认行为 MUST 明确定义，MUST NOT 静默用匿名登录。

### 6.2 安全默认

- FTPS 的证书校验 MUST 默认开启，MUST NOT 提供"默认忽略证书"的选项。
  用户可以显式关闭，但那次操作 SHOULD 被记录。
- SFTP 的 known_hosts 校验 MUST 默认开启。

### 6.3 与 HTTP 的差异

- FTP 没有 `Content-Type`，文件名**只能**从路径末段取（§4.2 的净化仍然适用）。
- FTP 的目录列表（`MLSD`）只在需要列出目录时使用，MUST NOT 在探测阶段做全目录扫描。

---

## 7. 失败分类的协议扩展

`engine-v1.md` §8.2 的十二类对四类协议**全部适用**，另加：

| 类别 | 触发 | 可重试 |
|---|---|---|
| `TrackerUnavailable` | tracker 不可达，但 DHT 仍在工作 | ✅ 且 SHOULD NOT 单独导致任务失败 |
| `NoPeers` | 一段时间内没有任何可用 peer | ⚠️ 可重试，但 MUST 有上限，MUST NOT 无限等待 |
| `MetadataUnavailable` | 磁力链接长时间取不到元数据 | ⚠️ 同上 |
| `PlaylistMalformed` | 播放列表/MPD 无法解析 | ❌ |
| `UnsupportedProtection` | 遇到 DRM | ❌ 且 MUST 与「解析失败」区分 |
| `AuthRejected` | FTP/SFTP 认证失败 | ❌ |
| `HostKeyMismatch` | SFTP 主机密钥不匹配 | ❌ **且 MUST NOT 提供"忽略并继续"的自动重试** |

**`NoPeers` 与 `MetadataUnavailable` 的上限是硬要求**：BT 最容易变成「永远卡在 0%」，
而用户无法区分「网络慢」与「这个种子根本没人做种」。

---

## 8. 与 M1 的兼容

- `engine-v1.md` §I5（单源退化）依然成立：单协议任务的行为与 M1 逐字节一致。
- 所有新协议的任务 MUST 能被 M1 的 CLI 以同一组命令驱动（`list` / `pause` / `resume` / `remove`）。
- `spec/cli-v1.md` 的退出码与状态隔离对多协议任务**不新增码**。

---

## 9. 变更记录

| 版本 | 日期 | 变更 |
|---|---|---|
| 1.0.0 | 2026-10-12 | 初始版本。覆盖 M2 的五类协议、统一任务模型与探测、失败分类扩展 |
