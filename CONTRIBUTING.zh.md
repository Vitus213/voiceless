# 参与 Voiceless 开发

感谢你的参与。本页说明开发环境、改动必须通过的检查、代码布局，以及几个
不知道就会浪费时间的坑。英文版见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 开发环境

一切通过仓库根目录的 Nix flake 运行；Rust crate 本身在 `rust/`。

```bash
nix develop            # CUDA 开发 shell（默认）：cargo、clippy、rustfmt、pipewire、wtype、wl-clipboard、Sherpa-ONNX、CUDA 工具链
nix develop .#cpu      # 纯 CPU shell：CI 用的就是它，不下载 CUDA
```

两个 shell 都导出 `SHERPA_ONNX_LIB_DIR` 与 `LD_LIBRARY_PATH`，让 `sherpa-onnx`
crate 链接 nixpkgs 的构建而不是自行下载二进制。CUDA shell 另外导出
`TRANSCRIBE_CMAKE_ARGS` 和 `VOICELESS_CUDA_LIB_DIRS` 供 transcribe.cpp 引擎使用
（`cargo build --features cuda`）。

`just` 封装了常用命令，并总是带上 `--manifest-path rust/Cargo.toml`：

| 配方 | 实际执行 |
| --- | --- |
| `just test` | 在 `nix develop` 内运行 `cargo test --all-targets` |
| `just lint` | `cargo fmt --check` 与 `cargo clippy --all-targets --all-features -D warnings` |
| `just check` | `just lint`、`just test`，再 `nix flake check -L`（沙箱内构建并测试 CPU 包，含 doctest） |
| `just run …` | `nix run . -- …` |
| `just bridge-test` | 在私有 D-Bus 会话里跑 Fcitx5 桥接端到端测试（`scripts/fcitx5-bridge-test.sh`） |

### 值得知道的坑

- **Clippy 用独立的 target 目录**（`rust/target/clippy`）。`sherpa-onnx-sys`
  的构建脚本会把只读的 Nix store 库复制进 profile 目录，第二次运行无法覆盖，
  所以 `cargo test` 与 `cargo clippy` 共用 target 目录会报 `Permission denied`。
  任何会重跑构建脚本的 cargo 命令（例如 `cargo doc`）都请换一个
  `CARGO_TARGET_DIR`。
- **用 shell 里的 `cargo`/`clippy`/`rustfmt`。** 用户 profile 里的 clippy 会拒绝
  shell 的 rustc 构建出的 crate（E0514）。
- **GPU 测试需要驱动。** `nix flake check` 只构建 CPU 包；GGUF 引擎请在本地跑
  `cargo test --features cuda`。
- **真实服务测试是 skip 不是 pass。** 名为 `real_*` 的测试与 `benchmark` 门禁
  需要凭据、已安装模型或 Wayland 会话；缺少时报告 `skip`/`unverified`，
  绝不报告通过。

### 没有麦克风也能跑会话

采集层从 `PATH` 启动 `pw-record`，所以端到端运行可以用假程序喂 WAV。在 `PATH`
上放一个可执行的 `pw-record`，把 16 kHz 单声道 PCM16 原始数据写到 stdout，并在
收到 SIGINT 时退出（要显式装信号处理器：脚本里用 `&` 启动的 daemon 会继承
`SIG_IGN`）。测试 Nix 安装的包时要调用 `bin/.voiceless-wrapped`，因为 wrapper
会把真正的 pipewire 放到 `PATH` 前面。把 `XDG_DATA_HOME` 指向临时目录（里面放一个
指向真实模型的 `models` 符号链接），测试会话就不会进入你的历史。

## 代码布局

```text
rust/src/
  main.rs            clap CLI、退出码                      lib.rs            crate 文档与模块地图
  coordinator.rs     一次会话：采集 → 传输 → 文本 → 注入 → 历史（唯一的副作用边界）
  session.rs         转写状态机                            capture.rs        pw-record + WavRecorder
  stream.rs          `stream` 命令：配置 → 后端（含备用）→ coordinator；各种注入方式
  realtime.rs        DashScope 实时 WebSocket              doubao.rs         火山引擎流式协议
  local_asr.rs       Sherpa-ONNX 流式 Paraformer           offline_asr.rs    VAD 分段的离线解码器
  gguf_asr.rs        transcribe.cpp GGUF 引擎（feature）   engine.rs         daemon 常驻的热引擎
  daemon.rs          gateway 状态机、D-Bus、热键状态机、控制台启动
  ctl.rs             `voiceless ctl` D-Bus 客户端          hotkey.rs         evdev 按住说话监听
  fcitx5.rs          Fcitx5 桥接客户端 + 预编辑 sink        media.rs          MPRIS 暂停、起止提示音
  text.rs            `[text]` 清理                         processing.rs     LLM 模式（openai-compatible、ollama）
  modes.rs           模式仓库（modes.json）                history.rs        SQLite 历史，schema v2
  models.rs          固定模型目录 + 安装器                 config.rs         严格 TOML schema、0600 门禁
  batch.rs / batch_cmd.rs       `transcribe`、`record`     doctor.rs         依赖检查
  benchmark*.rs / latency*.rs   CER 与延迟门禁             console/          axum 控制台（+ assets/console）
  *_cmd.rs           model / mode / history 的薄 CLI 适配层
rust/tests/          集成测试；`common/mod.rs` 放各种 fake（采集、传输、注入器、历史）
fcitx5-bridge/       C++ Fcitx5 插件，暴露 dev.voiceless.Fcitx5Bridge1
contrib/eww-overlay/ 由 `voiceless ctl watch` 驱动的 eww 浮层
nix/                 Home Manager 模块、Fcitx5 桥接包
docs/                架构、控制台 API、历史方案
openspec/            规格与变更提案
scripts/asr_benchmark/  `benchmark asr` 用的语料、TTS 生成与归一化
```

动会话路径之前先读 `docs/architecture.zh.md`，里面列出了测试强制的不变量
（有界队列、停止次序、一次注入、取消不注入）。

## 提交改动

1. **保住契约。** CLI 名称与选项、TOML 键、JSON 事件字段、模型 ID 与 D-Bus
   名称都是稳定标识。删除或重命名需要在 `CHANGELOG.md` 和 README 表格里说明。
   未知 TOML 键与已移除的旧值是故意拒绝的，不要加静默改写。
2. **行为先写测试。** 每个行为改动都在 `rust/tests/` 下用 `tests/common/mod.rs`
   的 fake 补集成测试；协议代码用抓到的帧补单元测试。需要真实服务的测试必须
   干净地 skip。
3. **文档同一提交。** 用户可见的改动要同时更新 `README.md` 与 `README.zh.md`，
   控制台端点更新 `docs/console-api.md`，新模块或不变量更新
   `docs/architecture.md` 与 `docs/architecture.zh.md`，并补 `CHANGELOG.md`。
4. **注释解释为什么。** 模块文档（`//!`）写明它在管道里的位置和保证；公开项
   写 `///`；超时、协议细节、规避手段写行内注释。doc 注释里不要放 Rust 代码块
   （`nix flake check` 会把它们当 doctest 编译）。
5. **提 PR 前跑 `just check`。** CI 跑的是同样的 `cargo fmt --check`、
   `cargo test --all-targets`（CPU shell）与 `nix flake check`。

### 提交信息

Conventional-commit 风格，祈使语气，一次提交一个改动：`feat: …`、`fix: …`、
`docs: …`、`test: …`、`ci: …`、`chore: …`，可带作用域（`fix(ci): …`）。标题里
点名受影响的配置段或命令会更清楚（`feat: [text] cleanup — …`）。

### Pull request

说明行为改动、如何验证（跑了哪些测试；涉及注入或热键时在哪个桌面验证）以及
契约变化。控制台改动的截图放 PR 里，除非 README 要用，否则不放 `docs/images`。

## 如何添加

- **流式后端**：实现 `BackendTransport`（见 `coordinator.rs`），在
  `stream::open_transport` 加 `"<name>"` 分支，在 `config.rs` 加带校验的配置段，
  为缺凭据加 `StreamError` 变体，在 `rust/tests/` 补协议测试，并在 README 的
  「配置」与「适配」加行。ready 门必须在采集开始前失败。
- **模型**：在 `models::catalog()` 加 `ModelSpec`，固定 URL、字节数、SRI
  SHA-256、归档成员白名单与 `license_status`，再让
  `offline_asr::OfflineFamily::for_model`（或流式路径）知道怎么加载。提交前
  独立核验哈希。
- **控制台端点**：`console/` 里的 handler、`docs/console-api.md` 的一行、
  `tests/console.rs` 里覆盖令牌与 `Host` 检查的测试，以及
  `rust/assets/console/app.js` 的页面代码。
- **配置键**：`config.rs` 里带默认值的字段、校验、两种语言的 README 键参考，
  以及用户需要编辑时的控制台设置页。

## 发布

目前没有发布流水线。用户按 revision 固定 flake，所以「发版」就是推到 `main`、
在使用方的 `flake.lock` 里提升 `voiceless` 输入、再 `home-manager switch`。
保持 `CHANGELOG.md` 最新，以后打 tag 可以直接从它出。
