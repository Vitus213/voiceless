# Spec Delta

## ADDED Requirements

### Requirement: 默认云端真流式识别

`voiceless stream` SHALL 默认使用 `cloud-realtime` 后端，在会话仍处于采集状态时持续把 16 kHz、单声道 PCM16 音频发送到同一云端实时会话并消费局部和确认结果。CLI MUST 在停止输入前发布变化的非最终 `transcript` 事件；相同文本 MUST 不重复发布，云端已确认片段 MUST 仅追加到 `confirmed_segments`。

#### Scenario: 停止前已经发送音频并发布局部文本

- **WHEN** 用户持续说话且云端实时会话返回变化的局部文本
- **THEN** 在任何停止请求之前，客户端 MUST 已发送对应音频块并输出一个或多个 `is_final: false` 的 `transcript` 事件

#### Scenario: 云端确认一个语音段

- **WHEN** 云端实时会话报告一个包含文本的完成段
- **THEN** CLI MUST 将该文本追加到 `confirmed_segments`，清除该段的可修订局部文本并继续接收后续语音

### Requirement: 停止冲刷与唯一最终结果

第一次 `SIGINT`、第二次快捷键激活或正常采集 EOF SHALL 停止接受新音频、恰好一次交付有效尾帧并向当前实时后端发送完成信号。Voiceless MUST 使用该实时会话返回的最终文本作为唯一权威文本，不得在停止后对整段会话发起 SenseVoice、Qwen 或云端批量校准。成功会话 MUST 输出恰好一个最终 `transcript`，随后依次输出 `finalized` 和 `completed`；启用注入时最终文本 MUST 最多注入一次。

#### Scenario: 快捷键停止冲刷当前云端会话

- **WHEN** 用户停止默认云端实时会话且服务端返回最终文本
- **THEN** Voiceless MUST 先交付采集尾帧、发送一次完成信号，再输出该文本的唯一最终事件并完成一次注入，且 MUST 不发起停止后的批量转写请求

#### Scenario: 正常停止时没有语音文本

- **WHEN** 用户停止会话但实时后端没有返回非空文本
- **THEN** Voiceless MUST 完成会话而不注入空文本，并 MUST 给出可诊断结果

#### Scenario: 强制取消

- **WHEN** 正常停止尚未完成时收到第二次 `SIGINT`，或会话收到 `SIGTERM`
- **THEN** Voiceless MUST 取消采集和后端会话、发布 `cancelled` 后再发布 `completed`，不得处理、记录或注入部分文本，并以取消状态退出

### Requirement: 显式本地流式后端

`voiceless stream --backend local-streaming` SHALL 使用本地原生在线识别状态产生局部、确认和最终文本，并 MUST 不建立云端 ASR 连接。所需本地模型未安装、损坏或 provider 不可用时，会话 MUST 报告错误并结束，不得静默切换到云端。

#### Scenario: 断网使用本地后端

- **WHEN** 网络不可用但所需本地模型有效，用户选择 `local-streaming`
- **THEN** CLI MUST 在语音输入期间发布局部文本并在停止后完成最终文本，且 MUST 不尝试云端连接

#### Scenario: 本地模型不可用

- **WHEN** 用户选择 `local-streaming` 但模型缺失、损坏或 provider 不可用
- **THEN** CLI MUST 发布可诊断 `error` 后发布 `completed`，不得注入文本或切换到 `cloud-realtime`

### Requirement: 后端失败不隐式切换

流式后端的鉴权、网络、协议、超时、模型或推理失败 MUST 保持所选后端的错误语义。Voiceless MUST 不因运行时失败把音频发送到另一个后端；若失败发生在任何最终注入之前，则 MUST 不注入部分结果。

#### Scenario: 云端鉴权失败

- **WHEN** `cloud-realtime` 在会话建立时收到鉴权失败
- **THEN** CLI MUST 发布一个不包含密钥的 `error` 和一个 `completed` 事件，不得启动本地识别或注入文本

#### Scenario: 云端中途断线

- **WHEN** 已发布局部文本后 WebSocket 在最终结果前不可恢复地断开
- **THEN** CLI MUST 报告会话失败、保留局部文本仅作显示并不得将其作为权威文本注入或写入成功历史

### Requirement: 破坏性实时配置切换

实时后端配置 SHALL 只接受 `cloud-realtime` 和 `local-streaming`。旧 `cloud-vad`、`sensevoice-vad` 以及停止后二次校准的 `asr.final_backend` 配置 MUST 被拒绝，而不是解释为新后端或自动重写。

#### Scenario: 旧实时配置被拒绝

- **WHEN** 用户配置 `streaming_backend = "cloud-vad"`、`"sensevoice-vad"` 或提供 `final_backend`
- **THEN** 配置加载 MUST 失败并指出迁移到 `cloud-realtime` 或 `local-streaming`，且 MUST 不启动采集

## MODIFIED Requirements

无。

## REMOVED Requirements

无。

## RENAMED Requirements

无。
