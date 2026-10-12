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
