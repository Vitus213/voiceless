# Windows / macOS 移植方案（研究稿）

> 状态：研究稿，2026-10-28，**未批准**。
> 目标：同一套 Voiceless 功能（实时识别、本地 + 云端模型、一次注入、历史、控制台、常驻 daemon）
> 在 Windows 与 macOS 上可用；Linux 行为不变。
> 参考实现：type4me（Swift/macOS）、Typeless（Electron，纯云端）、Open-Less（Rust 三平台）、
> Handy（Rust + Tauri 三平台）。

---

## 1. 结论摘要

1. **不要做真正的交叉编译。** 这个项目的构建闭包里有两个 C/C++ 依赖
   （sherpa-onnx 的预编译共享库、transcribe.cpp 的 CMake 源码构建）加 macOS 代码签名/公证，
   交叉工具链的维护成本远高于收益。GitHub 提供原生 runner
   （`windows-latest`、`macos-14`（arm64）、`macos-15-intel`（x86_64）），
   **直接用原生 runner 矩阵构建**，这是唯一稳的路线。
2. **识别管线可以原样复用。** 云端 WebSocket/HTTP 后端（`realtime.rs`、`doubao.rs`、`batch.rs`、
   `processing.rs`）与本地模型（sherpa-onnx、transcribe.cpp）都是跨平台的；
   移植成本集中在"平台服务"：采集、快捷键、注入、进程间控制、浮层/托盘。
3. **现有架构已经预留了接缝。** `coordinator.rs` 把采集、传输、处理、注入、历史、事件
   都定义成了 trait（`AudioCapture`、`BackendTransport`、`TextInjector`、`TextProcessor`、
   `HistoryRecorder`、`EventSink`），所以不需要重写会话内核，只需要按平台提供实现。
4. **今天这份代码一行都编译不到 Windows。** `evdev`（依赖 `nix`）与 `ashpd`
   （`std::os::fd`）在 Windows 上直接编译失败，`libc` 的 `kill`/`inotify` 只在 Unix 存在；
   `pw-record`、`wtype`、`wl-copy`、`fcitx5`、D-Bus 服务全是 Linux 假设。
5. **工作量量级**：平台层落地 + 三平台 CI + 打包签名，按单人估算 4–6 周（见 §7 分阶段）。

---

## 2. 现状：绑死 Linux 的地方

| 位置 | 绑定点 | 作用 |
| --- | --- | --- |
| `rust/src/capture.rs:46-71` | 子进程 `pw-record --rate 16000 --channels 1 --format s16 --raw` | 实时采集 16 kHz 单声道 PCM16 |
| `rust/src/batch.rs:438-447` | 同上，`record` 子命令 | 整段录音 |
| `rust/src/hotkey.rs` | `evdev` + inotify 监听 `/dev/input` | 按住说话 / 短按锁定的快捷键 |
| `rust/src/stream.rs:926-1100` | `wtype`、`wl-copy`/`wl-paste`、Fcitx5 D-Bus 桥 | 把最终文本打进焦点窗口 |
| `rust/src/fcitx5.rs`、`src/media.rs` | zbus（`org.fcitx.Fcitx5`、MPRIS） | 输入法预编辑、录音时暂停播放器 |
| `rust/src/daemon.rs` | zbus `dev.voiceless.Daemon` + ashpd `GlobalShortcuts` | 常驻进程控制面、桌面全局快捷键 |
| `rust/src/ctl.rs` | zbus 客户端 | `voiceless ctl start/stop/toggle/status/watch` |
| `rust/src/console/*` | axum + Unix 权限位 (`OpenOptionsExt`) | 本机网页控制台 |
| `contrib/eww-overlay` | eww + Wayland | 录音浮层 |
| `rust/src/models.rs:859` | `std::os::unix::fs::symlink` | 模型版本目录 |
| `rust/src/batch.rs:461-496`、`stream.rs:599-604` | `libc::kill`、`tokio::signal::unix` | Ctrl-C / SIGTERM 停止 |

`config.rs:684`、`history.rs:214`、`capture.rs:117` 已有 `cfg(unix)` 分支，可以直接沿用这个做法。

---

## 3. 目标架构

### 3.1 平台层

新增 `rust/src/platform/` 目录，每个能力一个 trait，Linux 实现保持今天的行为：

```
platform/
  mod.rs        编译期选择实现（cfg(target_os)）+ 共享类型
  capture.rs    trait AudioSource   { start() -> ChunkStream; stop() -> tail }
  hotkey.rs     trait HotkeySource  { press / release 事件流 }
  inject.rs     trait TextSink      { type_text(&str); paste(&str) }
  ipc.rs        trait DaemonIpc     { call(Ctl); watch() -> EventStream }
  overlay.rs    trait Overlay       { show(state); hide() }
  media.rs      trait MediaControl  { pause_playing(); resume(names) }
  perms.rs      trait Permissions   { check() -> Vec<Missing> }
```

现有的 `coordinator` trait 不动；`stream.rs` 的 `PwCapture` / `ConfiguredInjector` 变成
`platform::capture::default()` / `platform::inject::default()` 的一种实现。

### 3.2 每平台后端映射

| 能力 | Linux（保持现状） | Windows | macOS |
| --- | --- | --- | --- |
| 采集 | `pw-record` 子进程 | `cpal` 0.18 WASAPI + `rubato` 重采样到 16 kHz | `cpal` CoreAudio + `rubato`（不改设备硬件采样率） |
| 快捷键 | evdev（现有实现） | `handy-keys` 0.3：`WH_KEYBOARD_LL`，有 KEYUP、可吞键 | `handy-keys`：`CGEventTap`（active filter），需辅助功能权限 |
| 输入文本 | `wtype`（Wayland 虚拟键盘） | `enigo` 0.6 `SendInput` + `KEYEVENTF_UNICODE` | `enigo` 0.6 `CGEventKeyboardSetUnicodeString`（20 字符分片） |
| 剪贴板 | `wl-copy` / `wl-paste` | `arboard` 3.7 + `exclude_from_history` | `arboard` + `ConcealedType` 标记 |
| 粘贴按键 | `wtype -M ctrl v` 等 | `enigo` 发 Ctrl+V | `enigo` 发 ⌘V，**V 键位要用 `UCKeyTranslate` 解析**（Dvorak 等布局） |
| 输入法 | Fcitx5 桥接（保留） | 无（IME 与 SendInput 冲突，走粘贴兜底） | 无（同上） |
| 控制面 | D-Bus `dev.voiceless.Daemon` | 本地命名管道 `\\.\pipe\voiceless` | Unix socket `$TMPDIR/voiceless.sock` 或本机 TCP |
| 全局快捷键（次要） | Portal `GlobalShortcuts` | 不需要（LL hook 已覆盖） | 不需要 |
| 浮层 | eww | 托盘图标 + `WS_EX_NOACTIVATE` 无焦点窗口 | 菜单栏项（`NSStatusItem`）+ 非激活面板 |
| 暂停播放器 | MPRIS | SMTC（`GlobalSystemMediaTransportControlsSessionManager`）[推断] | `MRMediaRemoteSendCommand`（私有 API，MAS 不可用）或直接不做 |
| 通知/权限检查 | `doctor` 现有检查 | 麦克风隐私开关、UIPI 提醒 | `AXIsProcessTrusted`、`CGPreflightListenEventAccess`、`NSMicrophoneUsageDescription` |

### 3.3 关键约束（决定了实现方式）

- **WASAPI 采集没有 `AUTOCONVERTPCM`**（cpal `wasapi/device.rs` 注释明确写"Capture streams do not;
  only native formats will work"），所以必须在应用侧把 48 kHz 立体声 f32 降到 16 kHz 单声道。
- **CoreAudio 上 `cpal` 会改设备硬件采样率**（`set_physical_format` 失败时回退 `set_sample_rate`），
  所以固定请求 16 kHz 会重调整机音频设备——必须用设备默认格式 + `rubato`。
- **macOS 注入需要辅助功能授权**，否则 `CGEventPost` 静默失败；`enigo::new()` 会在未授权时报
  `NoPermission`，正好可以在 `doctor` 里前置检查。
- **macOS 快捷键**：只监听用输入监控权限，可吞键的 filter tap 需要辅助功能权限；
  tap 被系统因超时禁用后必须重建（type4me 与 handy-keys 都做了这件事）。
- **Windows 注入受 UIPI 限制**：`SendInput` 打不进更高完整性级别的窗口（管理员程序），
  返回值也不会明说；`doctor` 应提示这一限制，而不是假装成功。
- **Windows 快捷键**：`RegisterHotKey` 只有按下、没有抬起，push-to-talk 不可用；
  必须走 `WH_KEYBOARD_LL`（全局钩子，回调必须在 1 秒内返回，否则被系统静默摘除）。
- **D-Bus 在 Windows/macOS 上没有可依赖的会话总线**，控制面必须换成本地 socket；
  `ctl` 客户端按平台选 transport，命令与 JSON 事件格式保持不变。

---

## 4. 依赖与 Cargo 组织

今天 `rust/Cargo.toml` 里这些依赖必须按目标平台切分：

```toml
[target.'cfg(target_os = "linux")'.dependencies]
evdev = { version = "0.13", features = ["tokio"] }
ashpd = "0.11"
zbus = "5.19"
libc = "0.2"

[target.'cfg(any(target_os = "windows", target_os = "macos"))'.dependencies]
cpal = "0.18"
rubato = "5"
enigo = "0.6"
arboard = { version = "3.7", default-features = false }

[target.'cfg(target_os = "windows")'.dependencies]
handy-keys = "0.3"
windows = { version = "0.62", features = ["Win32_UI_Input_KeyboardAndMouse", "Win32_Foundation"] }

[target.'cfg(target_os = "macos")'.dependencies]
handy-keys = "0.3"
objc2-core-graphics = "0.3"
objc2-application-services = "0.3"
```

其它依赖的平台状态：

| 依赖 | Windows | macOS | 说明 |
| --- | --- | --- | --- |
| `sherpa-onnx` 1.13.3 (`shared`) | ✅ x64：构建期自动下载 `win-x64-shared-MT-Release` | ✅ `osx-arm64` / `osx-x64` | 只有 `static` / `shared` 两个 feature，互斥；**arm64 Windows 不被 build.rs 自动识别**，得设 `SHERPA_ONNX_LIB_DIR` |
| `transcribe-cpp` 0.4.0 | ✅ 源码构建，MSVC + Ninja | ✅ arm64 CI 覆盖；Metal 是默认 feature | 需要 CMake ≥ 3.16 + C++17；本项目 `default-features = false`，macOS 要显式加 `metal` |
| `rusqlite` (`bundled`) | ✅ 需要 cl.exe（VS 预装） | ✅ Xcode CLT | 无需外部 SQLite |
| `zbus` | ⚠️ 能编译但没总线 | ⚠️ 同左 | 仍然只在 Linux 用 |
| `evdev` | ❌ `nix` 是 Unix-only | ❌ 无 `/dev/input` | 必须 cfg 隔离 |
| `ashpd` | ❌ `std::os::fd` | ⚠️ 无 portal | 必须 cfg 隔离 |

`sherpa-onnx` 的 Windows 预编译包是 `/MT`（静态 CRT），与 Rust MSVC 默认的动态 CRT 混用
需要留意；`shared` 模式下 CRT 封装在 DLL 内，通常无冲突（推断，需在 CI 里实测）。

### 模型分发

- **本地模型不进安装包**：沿用现在的 `voiceless model install`（下载 + SRI 校验 + 解压），
  路径从 XDG 改为平台数据目录（Windows `%LOCALAPPDATA%\voiceless`，macOS
  `~/Library/Application Support/voiceless`）。
- **`sherpa-onnx` 的 dylib/DLL 必须随包分发**：build.rs 只把它们复制到 `target/<profile>`，
  打包时要显式收进产物目录。

---

## 5. GitHub Actions 构建矩阵

### 5.1 为什么不是 cross-compilation

- 需要交叉的不仅是 Rust：`transcribe-cpp` 走 CMake 源码构建，`sherpa-onnx` 下载的是
  **按目标平台预编译**的库（linux/macos/windows 各一套），`rusqlite` 需要目标平台的 C 编译器。
- macOS 产物要 `codesign` + `notarytool`，这两个只能在 macOS 上跑。
- GitHub 已经有原生 Windows / macOS runner，成本几乎为零。

### 5.2 矩阵

| 平台 | runner | target | 产物 |
| --- | --- | --- | --- |
| Linux x86_64 | `ubuntu-latest` | `x86_64-unknown-linux-gnu` | 现有 Nix 包（不变），另加 `cargo` 构建的裸二进制 |
| Windows x86_64 | `windows-latest` | `x86_64-pc-windows-msvc` | `voiceless.exe` + `sherpa-onnx.dll` 等 |
| macOS arm64 | `macos-14` | `aarch64-apple-darwin` | 可执行文件 + dylib |
| macOS x86_64 | `macos-15-intel` | `x86_64-apple-darwin` | 同上 |
| macOS universal | `macos-14` | 两个 target + `lipo` | 合并后的通用二进制 |

> 注：macOS 上 arm64 runner 可以交叉到 x86_64（Xcode SDK 同时含两个架构），
> 但**第三方 dylib 也要一起 `lipo`**，否则通用包里会混进单架构依赖。稳妥做法是两个 runner
> 各出一个单架构包，再在 `macos-14` 上 `lipo` 合并。

### 5.3 工作流草案

```yaml
name: Build

on:
  workflow_dispatch:
  push:
    tags: ["v*"]
  release:
    types: [published]

permissions:
  contents: write

jobs:
  build:
    name: ${{ matrix.name }}
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - name: linux-x86_64
            os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
          - name: windows-x86_64
            os: windows-latest
            target: x86_64-pc-windows-msvc
          - name: macos-arm64
            os: macos-14
            target: aarch64-apple-darwin
          - name: macos-x86_64
            os: macos-15-intel
            target: x86_64-apple-darwin
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: rust -> target
          key: ${{ matrix.target }}

      - name: Install Linux audio headers
        if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev pkg-config

      - name: Build
        working-directory: rust
        run: cargo build --release --locked --target ${{ matrix.target }}

      - name: Test
        working-directory: rust
        run: cargo test --release --locked --target ${{ matrix.target }} --lib

      - name: Package (Linux)
        if: runner.os == 'Linux'
        run: |
          mkdir -p dist
          tar -C rust/target/${{ matrix.target }}/release -czf dist/${{ matrix.name }}.tar.gz voiceless

      - name: Package (Windows)
        if: runner.os == 'Windows'
        shell: pwsh
        run: |
          New-Item -ItemType Directory -Force dist | Out-Null
          $dir = "rust/target/${{ matrix.target }}/release"
          Copy-Item "$dir/voiceless.exe" dist/
          Get-ChildItem $dir -Filter *.dll | Copy-Item -Destination dist/
          Compress-Archive -Path dist/* -DestinationPath "dist/${{ matrix.name }}.zip"

      - name: Package (macOS)
        if: runner.os == 'macOS'
        run: |
          mkdir -p dist
          dir="rust/target/${{ matrix.target }}/release"
          cp "$dir/voiceless" dist/
          cp "$dir"/*.dylib dist/ 2>/dev/null || true
          tar -C dist -czf "dist/${{ matrix.name }}.tar.gz" .

      - uses: actions/upload-artifact@v4
        with:
          name: ${{ matrix.name }}
          path: dist/*
          if-no-files-found: error

  # 可选：把两个 macOS 单架构包合并成 universal
  universal:
    needs: [build]
    runs-on: macos-14
    steps:
      - uses: actions/download-artifact@v4
        with:
          path: artifacts
      - name: lipo
        run: |
          # 单架构包是 tar.gz，先解开再合并；二进制与每个 dylib 都要 lipo，
          # 否则通用包里会混进单架构依赖。
          mkdir -p arm x86 out
          tar -C arm -xzf artifacts/macos-arm64/macos-arm64.tar.gz
          tar -C x86 -xzf artifacts/macos-x86_64/macos-x86_64.tar.gz
          for f in ./arm/voiceless ./arm/*.dylib; do
            [ -e "$f" ] || continue
            lipo -create "$f" "x86/${f#./arm/}" -output "out/${f#./arm/}"
          done
      - uses: actions/upload-artifact@v4
        with:
          name: macos-universal
          path: out/*
```

### 5.4 签名与分发（可选阶段）

| 平台 | 最低要求 | 完整方案 |
| --- | --- | --- |
| macOS | 未签名包 + 文档写 `xattr -cr`（Open-Less 就是这么发的） | Developer ID Application 证书 + `codesign --options runtime --timestamp` + `notarytool submit --wait` + `stapler staple`，再配 Homebrew tap |
| Windows | 未签名 zip + SmartScreen 提示 | Authenticode（`signtool sign /fd SHA256 /tr ... /td SHA256`），EV 证书可免 SmartScreen 警告 |
| Linux | 现有 Nix flake | 不变 |

需要预置的 secrets：`APPLE_CERTIFICATE_P12` / `APPLE_CERTIFICATE_PASSWORD` / `APPLE_ID` /
`APPLE_TEAM_ID` / `APPLE_APP_PASSWORD`、`WINDOWS_CERT_PFX` / `WINDOWS_CERT_PASSWORD`。
没有这些也能出包，只是用户首次运行要手动放行。

---

## 6. 参考实现的做法

| 项目 | 采集 | 快捷键 | 注入 | 识别 | 分发 |
| --- | --- | --- | --- | --- | --- |
| **type4me**（Swift） | `AVCaptureSession` + `AVAudioConverter` → 16 kHz mono | 主动 `CGEventTap`（可吞键） | 剪贴板 + 合成 ⌘V，300 ms 后还原 | sherpa-onnx C API（SenseVoice）+ MLX Qwen3 + 18 家云厂商 | Developer ID 签名 + 公证 + Homebrew tap（GitHub Action 自动更新 cask） |
| **Typeless**（Electron） | `getUserMedia` + AudioWorklet，Opus 32 kbps | 原生 helper 的 `CGEventTap` | 剪贴板 + ⌘V（`UCKeyTranslate` 解析 V 键位） | **纯服务端**，没有本地模型 | electron-updater |
| **Open-Less**（Rust） | 未细看 | `CGEventTap` / `WH_KEYBOARD_LL` | 剪贴板兜底 | — | ad-hoc 签名 + 自建 Homebrew tap |
| **Handy**（Rust + Tauri） | `cpal` + `rubato` | — | wtype → dotool → ydotool 依次回退 | sherpa-onnx | Tauri 打包 |

对本案的三点启发：

1. **注入只用"剪贴板 + 粘贴键"就够**，不必逐字符合成；type4me 与 Typeless 都是这么做的，
   逐字符只作为兜底。这条对中文长文本更稳（IME、组合字符都不会丢）。
2. **快捷键必须自己抓原始按下/抬起**，系统级注册热键（`RegisterHotKey`、Carbon
   `RegisterEventHotKey`）拿不到抬起事件，做不了按住说话。
3. **本地模型完全可以跨平台**：sherpa-onnx 官方支持 Win/macOS/Android/iOS，
   type4me 在 macOS 上就是直接调它的 C API。

---

## 7. 分阶段计划

| 阶段 | 内容 | 验收 |
| --- | --- | --- |
| **M0（1–2 天）** | Cargo 按平台切依赖；`platform/` 模块骨架 + `cfg` 选择；`libc`/`std::os::unix` 全部收进 Unix 分支 | Linux `cargo test` 全绿；`cargo check --target x86_64-pc-windows-msvc` 能过类型检查（CI 上验证） |
| **M1（1 周）** | Windows：cpal 采集 + handy-keys 快捷键 + enigo/arboard 注入 + 命名管道控制面；关闭 fcitx5/MPRIS/portal | 真机（或 CI 冒烟）跑通"按 ScrollLock → 说话 → 文本进入记事本" |
| **M2（1 周）** | macOS：同上 + 权限引导（麦克风/辅助功能/输入监控）+ 菜单栏图标；`doctor` 增加权限检查 | 同上，文本进入 TextEdit |
| **M3（3–5 天）** | 三平台 CI 矩阵 + 产物打包 + 可选签名/公证 + README/兼容性表更新 | 打 tag 能出四个平台的包 |
| **M4（可选）** | 托盘/浮层 UI、SMTC 暂停播放、全局快捷键 toggle、Windows arm64 | 按需 |

---

## 8. 风险与待定

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| macOS 权限模型（辅助功能/输入监控/麦克风） | 没有授权时功能静默失效 | `doctor` 主动检测并给出一键跳转 `x-apple.systempreferences:` |
| Windows UIPI | 打不进管理员窗口 | 文档说明；可选提供 `uiAccess` 签名版本 |
| `sherpa-onnx` 的 `/MT` 静态 CRT 与 Rust MSVC 动态 CRT | 链接冲突 | 先用 `shared`，CI 里实测；必要时 `+crt-static` |
| `cpal` 0.19 破坏性变更 | 升级成本 | 锁 `0.18.x` |
| Wayland 上 cpal 依赖 ALSA | 可能和 PipeWire 抢设备 | Linux **继续用 `pw-record`**，不切 cpal |
| 代码签名证书 | 没有证书就出不了"双击即用"的包 | 未签名 + 文档说明，后续再补 |
| 模型体积 | 本地模型 ×3 平台 | 不进安装包，`model install` 按需下载 |

待定问题：
- 控制面到底用本地 socket 还是"每次调用都是一个短命进程直接干活"（现在的 `stream` 子命令
  其实可以不依赖 daemon）；如果不需要常驻预热模型，IPC 可以整个砍掉，工作量再降一截。
- macOS 是否需要 `NSStatusItem` 之外的浮层？type4me 有浮动条形窗口，Typeless 有黑色方块浮层
  （用户甚至专门写了工具去修它）——这块容易做成负资产，建议 M4 再说。
