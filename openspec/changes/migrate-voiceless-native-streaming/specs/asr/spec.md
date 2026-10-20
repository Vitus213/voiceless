# Spec Delta

## ADDED Requirements

### Requirement: 云端批量识别后端继续可用

批量后端 `cloud` MUST 继续通过 DashScope HTTP 模型返回文本；Rust 迁移 MUST 保留批量 `transcribe`/`record` 的云端选择、鉴权错误、网络错误和重试边界。该后端不属于实时 `cloud-realtime` 会话，且不得被停止冲刷路径调用。

#### Scenario: 批量 cloud 继续工作

- **WHEN** 用户配置 `asr.batch_backend = "cloud"` 并调用 `voiceless transcribe <wav> --backend cloud`
- **THEN** 命令 MUST 返回云端转写文本并标记 `backend == "cloud"`，且 MUST 使用批量接口而非实时会话

#### Scenario: 批量 cloud 缺少密钥

- **WHEN** 用户选择批量 `cloud` 但没有有效 API key
- **THEN** 命令 MUST 在本地识别前返回不含密钥值的鉴权错误，不得静默切换到本地后端

## MODIFIED Requirements

### Requirement: 实时后端枚举与语义

实时后端枚举 MUST 为 `{ "cloud-realtime", "local-streaming" }`。`cloud-realtime` MUST 表示一个在采集期间持续发送 PCM、接收局部/确认结果并在停止时显式 finish 的实时会话；`local-streaming` MUST 表示完全本地的原生在线识别会话。旧 `cloud-vad` 和 `sensevoice-vad` MUST 不再被接受或自动重写。

#### Scenario: 默认实时后端为 cloud-realtime

- **WHEN** 用户未覆盖实时后端并启动 `voiceless stream`
- **THEN** 配置 MUST 选择 `cloud-realtime`，在 ready 前建立实时会话，且不得使用本地 VAD 分段 HTTP 转写

#### Scenario: 旧 cloud-vad 配置被拒绝

- **WHEN** 配置包含 `streaming_backend = "cloud-vad"` 或 `"sensevoice-vad"`
- **THEN** 配置加载 MUST 失败并给出迁移到 `cloud-realtime` 或 `local-streaming` 的诊断，不得启动采集

#### Scenario: 显式 local-streaming 不建立云端连接

- **WHEN** 用户执行 `voiceless stream --backend local-streaming`
- **THEN** 会话 MUST 只使用有效本地在线模型，不得创建 DashScope WebSocket 或批量 HTTP 请求

## REMOVED Requirements

### Requirement: 现有最终校准后端枚举扩展

Reason: Rust 真流式会话在停止时直接取得同一实时会话的权威最终文本；保留 `final_backend` 会重新引入停止后的整段校准和不确定的额外延迟。

Migration: 删除 `asr.final_backend` 配置和 `cloud`/`qwen3-sherpa` 最终校准选择。使用 `cloud-realtime` 或 `local-streaming` 产生唯一最终文本；模式处理只在最终文本之后执行。旧配置 MUST 在加载时报告迁移诊断，不自动重写。

## RENAMED Requirements

无。
