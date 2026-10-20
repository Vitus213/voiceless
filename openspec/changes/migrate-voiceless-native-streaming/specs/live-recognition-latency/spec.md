# Spec Delta

## ADDED Requirements

### Requirement: 完整且有界的低延迟音频输入

Voiceless SHALL 默认以 16 kHz、单声道 PCM16 和 32 ms 采集块处理实时语音，并把每个完整音频块按采集顺序恰好交付一次。非空且 PCM16 对齐的 EOF 或停止尾帧 MUST 在后端完成信号之前交付；奇数字节尾帧 MUST 产生可诊断错误且不得注入文本。实时传输积压 MUST 有固定上限；超过上限或传输截止时间时 MUST 使会话失败，而不是丢弃、重排音频或无限增长内存。

#### Scenario: 默认实时采集块

- **WHEN** 用户使用默认 capture 配置启动 `voiceless stream`
- **THEN** 采集和后端输入 MUST 使用 16 kHz、单声道 PCM16 及 32 ms 块

#### Scenario: 对齐尾帧先于完成信号

- **WHEN** 正常停止产生非空且 PCM16 对齐的尾帧
- **THEN** 该尾帧 MUST 恰好一次到达所选后端，随后才能发送后端完成信号

#### Scenario: 不完整 PCM16 尾帧

- **WHEN** 实时采集以奇数字节数的尾帧结束
- **THEN** 会话 MUST 报告可诊断错误，且不得发布最终文本或执行注入

#### Scenario: 网络发送持续慢于采集

- **WHEN** 云端传输积压达到配置的固定上限或单个音频块超过发送截止时间
- **THEN** 会话 MUST 失败并停止采集，不得静默丢弃音频、重排音频或继续无限缓存

### Requirement: 目标环境端到端延迟门禁

仓库 SHALL 提供真实音频重放和真实目标环境验证流程。对 `quick` 模式默认 `cloud-realtime` 后端，验收 MUST 要求开始说话到首个非空局部文本 p95 不超过 1.0 秒，停止请求到最终文本 p50 不超过 0.6 秒，停止请求到实际注入完成 p99 不超过 1.0 秒。报告 MUST 分别记录采集、传输完成、服务端最终事件、模式处理和注入时间戳；无真实服务、Wayland 注入目标或足够样本时 MUST 标记未验证，不得宣称通过。

#### Scenario: 真实云端和 Wayland 基准通过

- **WHEN** 维护者在目标 DashScope 区域、目标网络和 Wayland 会话中执行至少 100 次停止到注入试验
- **THEN** 报告 MUST 包含逐样本时间戳、网络 RTT、模型、音频块大小、p50/p95/p99，并且全部延迟阈值 MUST 满足要求

#### Scenario: 非 quick 模式不冒充低延迟结果

- **WHEN** 模式包含外部润色、翻译或提示词处理
- **THEN** 报告 MUST 单独记录处理时延，且 MUST 不把该结果计入 quick 模式 1 秒门禁

#### Scenario: 环境不足时运行确定性测试

- **WHEN** CI 没有真实云端凭据、目标网络、麦克风或 Wayland 注入目标
- **THEN** CI MUST 明确跳过真实门禁、继续运行协议和生命周期测试，并 MUST 不产生“端到端延迟已通过”的结论

### Requirement: 实时识别质量门禁

仓库 SHALL 使用版本化、dev/test 分离且带参考文本的语料度量实时后端最终文本。默认 `cloud-realtime` 在保留测试集上的内容 CER MUST 不超过 0.02；报告 MUST 包含逐样本错误、失败数、模型标识和语料版本。本地后端 MUST 单独报告，不得用云端结果代替其质量数据。

#### Scenario: 云端实时质量基准

- **WHEN** 维护者对保留测试集运行 `cloud-realtime` 重放基准
- **THEN** 报告 MUST 显示内容 CER、逐样本结果和失败数，且内容 CER MUST 不超过 0.02

#### Scenario: 本地后端单独报告

- **WHEN** 维护者运行 `local-streaming` 基准
- **THEN** 报告 MUST 标识本地模型/provider 并给出独立 CER，不得合并或替换默认云端门禁

## MODIFIED Requirements

无。

## REMOVED Requirements

无。

## RENAMED Requirements

无。
