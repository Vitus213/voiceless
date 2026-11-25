# ASR 规格增量

## MODIFIED Requirements

### Requirement: 现有批量后端枚举扩展

`asr.batch_backend` SHALL 支持 `cloud` 和 `local`。既有 `voiceless transcribe` 与 `record` 的命令参数和默认选择 MUST 保持兼容。批量识别与实时听写是独立入口。

#### Scenario: 枚举包含 cloud

- **WHEN** 用户用 `--backend cloud` 转写 WAV，且 DashScope 凭据和连接有效
- **THEN** 命令使用已配置的云端批量接口并报告 `cloud`，实时停止不调用该接口

#### Scenario: 本地批量和非法后端值

- **WHEN** 用户选择 `local`，或传入已停用的批量后端名
- **THEN** 有效本地配置使用本地模型，未知后端在采集或传输前给出可用值和错误

### Requirement: 现有流式后端枚举扩展

`asr.streaming_backend` SHALL 支持 `cloud-realtime`、`doubao-realtime`、`local-streaming` 和 `local-offline`。配置的备用后端使用同一集合。未显式修改配置时，现有 CLI 默认值保持兼容。

#### Scenario: 执行已有实时配置

- **WHEN** 用户使用已存在的 Rust 配置开始听写
- **THEN** 有效配置使用所选实时后端，省略该选项时沿用原 CLI 默认值

#### Scenario: 枚举包含 cloud-vad

- **WHEN** 用户加载仍含 `cloud-vad`、`sensevoice-vad` 或 `asr.final_backend` 的旧配置
- **THEN** 在采集前拒绝旧配置，并显示可用的实时后端和迁移说明

## REMOVED Requirements

### Requirement: 现有最终校准后端枚举扩展

**Reason**: 现行 Rust 听写从所选会话取得权威最终文本。`asr.final_backend` 已被拒绝。

**Migration**: 移除 `asr.final_backend`，在 `asr.streaming_backend` 选择支持的实时识别后端。需要更换模型重转写时，从历史或文件任务显式发起。

## ADDED Requirements

### Requirement: 分别报告识别回退和计算设备回退

运行时 MUST 报告请求设备、有效推理设备、实际后端、模型和回退原因。预览与最终识别的信息 MUST 可分别读取。`auto` 可在本地硬件不可用时用 CPU，指定 CUDA 或 Vulkan 的引擎加载失败时报告所需硬件和原因。

#### Scenario: 自动模式选中 GPU 或 CPU

- **WHEN** Vulkan 或 CUDA 可用时以 `auto` 加载 GGUF，再在没有适用驱动的环境加载同一配置
- **THEN** 状态分别报告实际 GPU 和 CPU，记录回退原因，硬件枚举或显存占用不能作为模型使用该设备的唯一依据

#### Scenario: 指定设备不匹配

- **WHEN** 用户指定不可用的 Vulkan 或 CUDA，或为仅支持 CPU 的模型指定 GPU
- **THEN** 报告加载失败及原因，不将该请求显示为已在 GPU 上运行，也不把本地设备错误当成云端回退

### Requirement: 识别服务选择遵循用户授权

本地路径 MUST 在离线时可用，且只在用户显式配置云端主服务或备用服务时才发送语音。备用服务 MUST 只在主服务未接收会话音频的启动阶段启用，并报告主服务错误和实际选用的服务。

#### Scenario: 启动前启用已配置备用服务

- **WHEN** 主服务在接收会话音频前不可用，且有效配置明确启用了一个备用服务
- **THEN** 可尝试一次所选备用服务，客户端和结果都显示实际后端及回退原因

#### Scenario: 无授权或会话中途失败

- **WHEN** 用户只选择本地处理，或已发送语音的后端在会话中途失败
- **THEN** 不会把录音自动转发到云端或其他后端，故障可诊断且部分文本不会冒充最终结果

### Requirement: 模型选择与完整性校验

模型列表 MUST 提供语言、识别类型、所需空间、可用设备和进度。托管模型必须验证完整性后才能激活。用户可导入明确选中的可兼容本地模型，并保留现有已安装和正在使用的模型。

#### Scenario: 安装、导入和切换兼容模型

- **WHEN** 用户选定一个适用的模型完成下载，或显式选中一个兼容的本地 GGUF 文件
- **THEN** 模型经校验后可供选用，进度和实际生效状态可区分，现有模型继续可用直到新模型成功切换

#### Scenario: 下载中断或模型校验失败

- **WHEN** 下载失败、空间不足，或选中的模型不完整、不兼容
- **THEN** 给出可重试的原因，只能激活已完整校验的版本，正在使用的模型不会被误删
