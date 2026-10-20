## Why

当前 Python 实时路径仍以本地 VAD 切段后重复离线解码，云端路径也只在片段或整段结束后发起 HTTP 请求。即使批量 `qwen3-asr-flash` 已实测达到 0.0110 内容 CER 和 0.64 秒平均请求时延，用户停止说话后仍需等待上传与整段识别，无法稳定满足“停止快捷键后约 1 秒内输出”的核心任务。

2026-08-14，用户明确批准将项目破坏性重写为 Rust CLI：说话期间持续采集、上传并接收局部文本，停止时仅冲刷最后尾包和服务端最终结果；云端真流式负责默认质量路径，本地原生流式作为显式离线后端。该批准同时取代本 change 先前“Python + 本地 Paraformer 局部文本 + 停止后 SenseVoice/Qwen 批量校准”的技术方向。

## What Changes

- **BREAKING**：以原生 Rust `voiceless` 可执行文件替换 Python 分发、`type4me-linux` 命令、Python 包和 Adwaita 桌面应用；不提供旧命令、Python 导入、XDG 路径或流式后端名称的兼容别名。
- **BREAKING**：默认实时后端改为 `cloud-realtime`，使用 DashScope `qwen3-asr-flash-realtime` WebSocket 在采集期间持续发送 PCM 并接收局部/确认文本；停止时发送显式 finish 并以同一流式会话的最终结果作为唯一权威文本，不再执行停止后的 SenseVoice/Qwen/HTTP 整段校准。
- 新增显式 `local-streaming` 后端，使用 Sherpa-ONNX 原生在线识别器和本地端点检测提供断网路径；云端和本地失败均不得静默切换到另一后端。
- 保留 `stream`、`transcribe`、`record`、`model`、`doctor`、`mode`、`history` 和 headless `daemon` 核心 CLI 工作流，迁移严格 TOML、模型完整性、历史、模式、Portal 快捷键和一次最终注入语义。
- 建立真实实时重放基准，分别度量首个局部文本、停止到最终结果、最终结果到注入完成和内容 CER；只有真实目标环境结果满足门禁时才可宣称低延迟完成。
- 使用 Nix 管理完整 Rust 构建环境、Sherpa-ONNX/ONNX Runtime、PipeWire 与可选 CUDA；最终运行时不依赖 Python。

## Capabilities

### New Capabilities

- `voiceless-identity-migration`：以原生 Rust Voiceless 完成无兼容入口的身份、分发和持久化边界切换。
- `rust-cli-migration`：将核心 CLI、严格配置、模型管理、模式/历史和 headless 快捷键工作流迁移到 Rust，并明确移除 GUI。
- `native-cli-streaming-recognition`：提供边采集边上传/解码的云端真流式默认路径、显式本地流式路径和唯一最终注入语义。
- `live-recognition-latency`：约束完整音频交付、背压、停止冲刷和可复现的延迟/准确率门禁。

### Modified Capabilities

无。仓库当前没有已归档的 OpenSpec 基线 capability；已完成但尚未归档的 `cloud-asr` change 作为当前实现证据，本 change 在 Rust 切换时明确替换其 `cloud-vad` 实时行为，同时保留批量 `cloud` 能力。

## Impact

受影响范围包括全部 Python 源码与测试、Nix flake/Home Manager、XDG 路径、CLI/JSON 消费方、D-Bus/Portal 快捷键、模型目录、历史与模式数据，以及依赖 `cloud-vad`、`sensevoice-vad`、`final_backend` 或 GTK 桌面应用的用户。旧 XDG 目录留在磁盘但不被 Voiceless 自动读取或修改；用户可回装旧发布物回滚。云端默认路径会持续把会话 PCM 发送到 DashScope，文档必须明确这一隐私边界；选择 `local-streaming` 时不得建立云端连接。
