# chua 踩坑记录

> **判断标准**：任何"花了十分钟以上才搞明白"的问题——工具链限制、隐藏契约、误判过的行为、
> 与文档不符的实际情况。
> **四段式**：现象 → 根因 → 处置 → 复发预防。
> 只记会再踩一次的坑；一次性手滑不记。修完立刻写。

---

## 索引

| 编号 | 一句话 | 影响面 |
|---|---|---|
| **P-001** | **本机编译失败的真因是 `%TEMP%` 权限，不是编译器内存——旧项目据此错误地放弃了本机构建** | **全局，已解除** |
| P-002 | PowerShell 把 cargo 的 stderr 当错误，构建成功也会得到 exit code 1 | 所有 cargo 命令 |
| P-003 | 本机 MSVC 是 VS2019 (14.29) + SDK 19041，但完全够用 | 工具链 |
| **P-004** | **`cargo build` / `check --lib` 不编译测试目标：改了结构体字段而没跑 `cargo test`，会以为自己是绿的** | **所有改动，已加门禁** |
| **P-005** | **PowerShell 的 `Get-Content \| Set-Content` 往返会重写整个文件，破坏 UTF-8 与行尾；对源码做批量改写必须用 .NET 的 `ReadAllText`/`WriteAllText`** | **批量改写脚本** |

---

## P-004 · `cargo build` 是绿的，`cargo test` 连编译都过不了

**现象**

给 `TaskRecord` 加了一个字段 `name_hint` 之后，`cargo build` 与 `cargo check -p chua-engine --lib`
都报 `Finished`，于是继续往下写。实际上 `cargo test` 根本编译不过：

```
error[E0063]: missing field `name_hint` in initializer of `model::TaskRecord`
   --> crates\engine\src\events.rs:314:9
```

同样地，`lib.rs` 里少一行 `pub mod engine;` 时，`cargo check --lib` 也一直显示成功——
那 40 KB 的 `engine/` 目录根本没有被编译过。

**根因**

`cargo build` 与 `cargo check --lib` 编译的是 **lib 目标**，不含 `#[cfg(test)]` 单元测试、
`tests/` 集成测试与 example。测试目标里的构造器与主代码不同步时，这两条命令完全看不见。

**处置**

门禁改成三条一起跑，缺一不可：

```powershell
cargo build
cargo test -p <crate> --no-run     # 先确认测试目标能编译
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

`--all-targets` 是关键：它把 test / example 目标纳入 lint 范围，所以 clippy 能抓住
`cargo build` 抓不到的这类错误。

**复发预防**

1. **每加一个模块就先在 `lib.rs` 里声明再写**，否则写出来的代码一行都没被编译过。
2. **改了任何结构体字段，立刻跑一次带 `--all-targets` 的命令**，不要等"写完再一起编"。
3. 报告构建结果时，说清跑的是哪条命令；`cargo build` 通过 **不等于** 测试目标能编译。

---

## P-005 · PowerShell 文本往返会把源码改成乱码

**现象**

用 `Get-Content x.rs -Raw` 读出来、`-replace` 改一处、再 `Set-Content` 写回去之后，
文件里所有非 ASCII 字符都变成了 `鈥` / `搂` 这样的乱码（`—` 与 `§` 全部损坏）。

**根因**

`Get-Content` 默认按系统 ANSI 代码页解码，`Set-Content` 再按同一代码页编码。
UTF-8 源码经这一趟往返必然损坏。而且损坏之后**肉眼看 `Select-String` 的输出是不可靠的**：
控制台本身的编码也会把正确的 UTF-8 显示成乱码（本项目里正确的 `§` 在终端里就显示成 `搂`），
所以"看到乱码"不能证明文件坏了，反过来也一样。

**判定办法**：不要看显示结果，直接看码点：

```powershell
$t = [System.Text.Encoding]::UTF8.GetString([System.IO.File]::ReadAllBytes($p))
$t.ToCharArray() | Where-Object { [int]$_ -gt 126 } | ForEach-Object { "U+{0:X4}" -f [int]$_ } | Sort-Object -Unique
```

**处置 / 复发预防**

批量改写源码一律走 .NET，并显式指定不带 BOM 的 UTF-8：

```powershell
$t = [System.IO.File]::ReadAllText($p, [System.Text.Encoding]::UTF8)
$t = $t.Replace($old, $new)
[System.IO.File]::WriteAllText($p, $t, (New-Object System.Text.UTF8Encoding($false)))
```

能用 `edit` 工具的单点改动就用它，不要为了省事做整文件往返。

---

## P-001 · 本机编译失败的真因是 `%TEMP%` 权限，不是编译器内存

**现象**

`cargo build` 在编译带 C/C++ 依赖的 crate（本次是 `vswhom-sys`）时失败：

```
c1xx: fatal error C1083: 无法打开编译器中间文件:
  "C:\Users\sn1p4\AppData\Local\Temp\_CL_55425307ex": Permission denied
error occurred in cc-rs: command did not execute successfully (status code exit code: 2):
  "...\cl.exe" ... "-c" "ext/vswhom.cpp"
```

旧项目里这个坑以 `cl.exe D8050` 的形态出现，并由此得出**「本机无法编译引擎，只能靠 CI 验证」**
的结论，把整个开发流程改成了"推送 → 等 3.5 分钟 → 从 ci-status 分支取结果"。

**根因**

`cl.exe` 需要在 `TMP` 目录里创建 `_CL_*` 中间文件。本机 `%TEMP%`
（`C:\Users\sn1p4\AppData\Local\Temp`）上存在一批外来 ACL 条目：

```
HA1O\CodexSandboxUsers:(OI)(CI)(M)
S-1-5-21-2515918534-3075528980-1570501591-1546790400:(OI)(CI)(M)
S-1-15-2-750044746-...-245317822:(OI)(CI)(M,DC)        ← AppContainer 包 SID
S-1-5-21-2668377347-2902022078-4043270077-1001:(I)(OI)(CI)(F)   ← 与本机 SID 不符
```

两条 `S-1-5-21-…` 的机器 SID 都与本机（`HA1O` = `2505847641-4085228434-2088558674`）不一致，
是别的机器遗留下来的。cargo 派生的**孙进程** `cl.exe` 在该目录创建中间文件时被拒。

**关键陷阱：简单的写探测会误导你。** 从 PowerShell 直接往 `%TEMP%` 写一个文件是**成功**的
（`PROBE OK`），残留的 `_CL_*` 文件数是 0。只有编译器那种"创建临时中间文件"的模式才触发。
所以"我能写 Temp，所以不是权限问题"是错误推理。

**处置**

把 `TMP`/`TEMP` 指向工作区内的目录再构建：

```powershell
$env:TMP  = "E:\dsh-workspace\tdmfast\.gpui-spike\.tmp"
$env:TEMP = $env:TMP
cargo build --release
```

结果：**整个 GPUI 依赖树 debug 41 秒、release 59 秒编译通过。**

**复发预防**

1. 本项目**所有** `cargo` 命令先设置 `TMP`/`TEMP` 到工作区内目录。
2. 再次见到 `D8050` 或 `C1083` + `Permission denied` 时，**先查 `TMP`**，
   不要去查内存、并行度、页面文件或杀软。
3. 不要因为一次编译失败就下"本机不能构建"的结论——先确认失败发生在**哪个**环节。
   旧项目的这个误判让整个开发循环付出了数周的 CI 往返代价。
4. **不只是 `cl.exe`——任何创建临时文件的东西都会中招。** 已复发的第二处是
   `tempfile::tempdir()`：**直接运行测试二进制**（而不是经 `cargo test`）时若没有设置
   `TMP`/`TEMP`，集成测试会在建临时目录那一步**全部**失败，报
   `PermissionDenied ... Temp\.tmpXXXXXX`。

   **这个形态极具误导性**：六个集成测试同时"失败"，看起来像引擎全线崩了，
   实际一行引擎代码都没执行到。识别信号是——**失败的测试都倒在同一步**（这里是
   `tests/common/mod.rs` 的同一个 helper），且错误文本里出现 `Temp\.tmp`。

   **规则：运行任何测试二进制前也先设 `TMP`/`TEMP`，不要只在 `cargo` 命令前设。**

---

## P-002 · PowerShell 把 cargo 的 stderr 当错误，构建成功也会得到 exit code 1

**现象**

`cargo build 2>&1 | Select-String ...` 之后，即使日志里明确写着 `Finished release profile ...`，
命令仍以 `[exit code: 1]` 结束，并伴随一段 `NativeCommandError`：

```
cargo :    Compiling buildtest v0.0.0 (...)
所在位置 行:23 字符: 1
+ cargo build 2>&1 | ...
    + CategoryInfo          : NotSpecified: (   Compiling ...:String) [], RemoteCommandError
```

**根因**

cargo 把**进度信息写到 stderr**（这是正常行为，不是错误）。PowerShell 在 `2>&1` 重定向时
把 stderr 的每一行包装成 `ErrorRecord`，于是整个管道以非零码收尾。

**处置**

**以日志内容判断成败，不以退出码判断。** 关注 `Finished` / `error[` / `error:` / `warning: build failed`。

**复发预防**

- 在 CI 或脚本里判断构建结果时，用 `cargo build --message-format=json` 或检查输出内容，
  不要只信 PowerShell 会话里的退出码。
- 报告构建结果时，引用的必须是日志里的判据，不能是 `[exit code: N]`。

---

## P-003 · 本机 MSVC 是 VS2019 (14.29) + SDK 19041，但完全够用

**现象**

担心 `rustc 1.95 (2026)` 配 `MSVC 14.29.30133 (VS2019 16.11, 2021 年)` 太旧会出问题。

**实际**

| 项 | 值 |
|---|---|
| 工具链 | `C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools` |
| MSVC | `14.29.30133`（唯一版本） |
| Windows SDK | `10.0.19041.0`（唯一版本） |
| 结果 | Rust 1.95 在其上编译链接**正常**，含 `windows-rs`、DirectX、`cc` 构建脚本 |

**处置 / 复发预防**

不要为"工具链太旧"去折腾升级 VS。真正的阻塞是 P-001 的 `%TEMP%` 权限。
