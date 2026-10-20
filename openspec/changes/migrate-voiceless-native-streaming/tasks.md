## 1. Rust 云端真流式 tracer bullet

- [x] 1.1 [Requirement: 原生 Rust CLI 分发；Scenario: 无 Python 环境运行核心 CLI] RED：为原生 `voiceless --help`、`doctor` 和核心子命令清单写 Rust CLI 行为测试，并让 Nix 包在无 Python 闭包下失败；GREEN：建立提交 `Cargo.lock` 的 Rust package、clap 入口和 `rustPlatform.buildRustPackage`，以最小可运行 `doctor` 闭合安装路径，Sherpa 使用 shared feature 与 `SHERPA_ONNX_LIB_DIR`，不得在构建期下载库；REFACTOR：收敛命令错误和退出码。验证：`nix develop -c cargo test --test cli_identity`、`nix build -L`。

- [x] 1.2 [Requirement: 严格配置与后端选择；Scenario: 未知配置在采集前失败；Scenario: 配置文件权限过宽；Requirement: 破坏性实时配置切换；Scenario: 旧实时配置被拒绝] RED：以临时 XDG 根覆盖默认、完整、未知键、非法 endpoint/model/VAD/timeout、宽权限密钥文件和旧 `cloud-vad`/`sensevoice-vad`/`final_backend`；GREEN：实现严格 TOML、0600 密钥门禁和 `cloud-realtime`/`local-streaming` 枚举，保证错误发生在 capture/transport 构造前；REFACTOR：让密钥使用 redacted 类型，禁止 Debug/Display 泄漏。验证：`nix develop -c cargo test --test config_contract`。

- [x] 1.3 [Requirement: 默认云端真流式识别；Scenario: 停止前已经发送音频并发布局部文本；Scenario: 云端确认一个语音段] RED：用进程内 DashScope 协议 WebSocket server、伪 `pw-record` 和事件 sink 证明 ready 前不采集、32ms PCM 在停止前已发送、变化 partial 发布、重复 partial 去重、completed 段只追加 confirmed；GREEN：实现 `pw-record --raw -` 捕获、单 Tokio 会话协调器、DashScope session.update/audio append/接收器和转写累加器；REFACTOR：生产后端枚举分派，协议解析与会话编排分离。验证：`nix develop -c cargo test --test cloud_realtime_streaming`。

## 2. 停止、失败与一次输出边界

- [x] 2.1 [Requirement: 停止冲刷与唯一最终结果；Scenario: 快捷键停止冲刷当前云端会话；Scenario: 正常停止时没有语音文本；Requirement: 完整且有界的低延迟音频输入；Scenario: 默认实时采集块；Scenario: 对齐尾帧先于完成信号] RED：记录 stop、捕获 SIGINT、尾帧、最后 audio append、session.finish、final/finalized/completed 和 injector 的调用顺序，覆盖空文本；GREEN：实现正常停止 drain 与唯一权威 final，禁止停止后批量校准，空文本不注入；REFACTOR：以单向状态转换和 once 门闩消除重复 finish/final/inject。验证：`nix develop -c cargo test --test session_finalize`。

- [x] 2.2 [Requirement: 停止冲刷与唯一最终结果；Scenario: 强制取消；Requirement: 后端失败不隐式切换；Scenario: 云端鉴权失败；Scenario: 云端中途断线] RED：覆盖第一次 SIGINT 后第二次 SIGINT、SIGTERM、连接期 401/403、partial 后断线和 finish 超时，断言事件顺序、无本地构造、无成功历史和无注入；GREEN：实现统一 Stop/Cancel、错误分类、子进程 kill-on-drop/等待和两秒功能超时；REFACTOR：所有错误路径共享资源清理但保留可诊断原因。验证：`nix develop -c cargo test --test session_failures`。

- [x] 2.3 [Requirement: 完整且有界的低延迟音频输入；Scenario: 不完整 PCM16 尾帧；Scenario: 网络发送持续慢于采集] RED：用奇数字节尾帧、容量 16 队列和阻塞发送器证明不丢块、不重排、不无界增长，达到 512ms 积压或 500ms send deadline 即失败且不注入；GREEN：实现固定容量 FIFO、截止时间和捕获停止传播；REFACTOR：音频块用拥有所有权的固定边界类型，避免不必要复制。验证：`nix develop -c cargo test --test audio_backpressure`。

- [x] 2.4 [Requirement: 核心数据工作流迁移；Scenario: quick 模式不调用文本处理服务；Scenario: 自定义模式处理失败] RED：覆盖 quick 零 HTTP 调用、模式模板展开、处理失败 warning/原文本保留、局部永不处理/注入、最终最多一次注入和历史只记录权威文本；GREEN：移植模式处理、wtype/剪贴板注入和历史写入到最终边界；REFACTOR：处理与注入作为 final-only pipeline，失败不复制或丢失识别文本。验证：`nix develop -c cargo test --test final_pipeline`。

## 3. 显式本地流式后端与核心数据 CLI

- [x] 3.1 [Requirement: 显式本地流式后端；Scenario: 断网使用本地后端；Scenario: 本地模型不可用；Requirement: 核心数据工作流迁移；Scenario: 显式检查发现模型损坏] RED：先固定上游模型 URL、字节数、SRI、允许/必需成员和许可证，再覆盖安装/check 损坏失效、断网 local partial/confirmed/final、零云端连接和缺模型/provider 错误不回退；GREEN：移植安全 ModelManager，使用 shared `sherpa-onnx` OnlineRecognizer/OnlineStream 实现 `local-streaming`；REFACTOR：daemon 复用 recognizer 但会话流隔离。验证：`nix develop -c cargo test --test model_manager --test local_streaming`；真实冒烟：`VOICELESS_REAL_LOCAL_ASR=1 nix develop -c cargo test --test real_local_asr -- --ignored`。

- [x] 3.2 [Requirement: 原生 Rust CLI 分发；Scenario: 无 Python 环境运行核心 CLI；Requirement: 核心数据工作流迁移] RED：为 `transcribe`、`record`、`model`、`doctor`、`mode`、`history` 的成功/错误/JSON 形状写 CLI 集成测试，并针对已实现 batch cloud 与本地后端建立 fixture；GREEN：逐命令迁移现有可观察行为，不迁入 GTK controller/view；REFACTOR：复用配置、模型、provider、事件和退出码边界，删除只为 Python 结构存在的抽象。验证：`nix develop -c cargo test --test cli_workflows`。

## 4. Headless 热键与 clean cutover

- [x] 4.1 [Requirement: Headless 快捷键会话控制；Scenario: 第二次快捷键激活正常停止；Scenario: 会话结束前重复开始] RED：以 fake Portal/D-Bus 和会话协调器覆盖 idle→recording→finalizing→idle、第二次激活 Stop、并发 Start 拒绝、Cancel 和一次注入；GREEN：用 zbus 移植 GlobalShortcuts 与公开控制总线到 `voiceless daemon`；REFACTOR：快捷键层只发送命令，不拥有 capture/backend。验证：`nix develop -c cargo test --test daemon_shortcuts`。

- [x] 4.2 [Requirement: Voiceless 唯一对外身份；Scenario: 新 CLI 和系统集成可识别；Requirement: 干净的持久化状态边界；Scenario: 仅存在旧状态目录时首次启动；Scenario: 用户需要迁移既有数据；Requirement: GUI 入口被移除；Scenario: GUI 入口被移除] RED：对 flake、Home Manager、desktop/D-Bus/Portal 元数据、新旧 XDG 隔离和迁移说明写安装级测试；GREEN：完成 Voiceless 标识切换，删除 Python `src/`、Python tests/pyproject、GTK/Adwaita 入口和旧别名，旧 XDG 保持不变；REFACTOR：清除废弃资源、模型引用和 Python/Nix 依赖。验证：`nix develop -c cargo test --test identity_cutover`、`nix flake check -L`、`nix build -L`。

## 5. 真实质量与端到端延迟门禁

- [x] 5.1 [Requirement: 实时识别质量门禁；Scenario: 云端实时质量基准；Scenario: 本地后端单独报告] RED：用确定性假结果验证 dev/test 隔离、逐样本错误、失败数、模型/provider/语料版本和 CER 聚合，确保云端与本地报告不可合并；GREEN：将现有语料重放为实时 32ms PCM，生成版本化 JSON 报告并对 `cloud-realtime` 执行 CER ≤0.02 门禁；REFACTOR：共享规范化/CER 逻辑，不共享后端结论。验证：`nix develop -c cargo test --test benchmark_contract`；真实门禁：`VOICELESS_REAL_CLOUD_ASR=1 nix develop -c cargo run --release -- benchmark asr --split test --backend cloud-realtime --enforce`。

- [x] 5.2 [Requirement: 目标环境端到端延迟门禁；Scenario: 真实云端和 Wayland 基准通过；Scenario: 非 quick 模式不冒充低延迟结果；Scenario: 环境不足时运行确定性测试] RED：用可控 clock 证明 start/first-partial/stop/tail-sent/finish-sent/final/injection 时间戳、quick/non-quick 分组、p50/p95/p99 和 skip 状态；GREEN：实现至少 100 次真实云端 + Wayland 注入基准，门禁 first partial p95 ≤1.0s、stop→final p50 ≤0.6s、stop→inject p99 ≤1.0s，缺环境时只输出 unverified/skip；REFACTOR：基准采集不改变稳定 RecognitionEvent JSON。验证：`nix develop -c cargo test --test latency_benchmark_contract`；真实门禁：`VOICELESS_REAL_E2E=1 nix develop -c cargo run --release -- benchmark latency --backend cloud-realtime --mode quick --trials 100 --inject --enforce`。

## 6. 完成验证

- [x] 6.1 [All Requirements / Scenarios] 更新 README、架构、配置与人工迁移说明；运行 `nix develop -c cargo fmt --check`、`nix develop -c cargo clippy --all-targets --all-features -- -D warnings`、`nix develop -c cargo test --all-targets`、`nix flake check -L`、`nix build -L`，并附上 5.1/5.2 的真实报告路径。任何真实门禁未运行或未满足时，任务 MUST 保持未完成且不得宣称"1 秒端到端已交付"。
