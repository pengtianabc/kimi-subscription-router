# ACP 多账号路由

`kimi-subscription-router` 是一个 stdio ACP multiplexer。ACP 客户端只启动这一个进程，
路由器为每个可用账号启动一个官方 `kimi acp` 子进程。

```text
ACP client
    |
    v
kimi-subscription-router
    |-- account A -> isolated KIMI_CODE_HOME -> kimi acp
    |-- account B -> isolated KIMI_CODE_HOME -> kimi acp
    `-- sessionId -> persisted account owner
```

## 路由规则

新会话优先使用在 7 天窗口重置前更需要消耗余量、同时仍有 5 小时窗口容量的账号。
分数接近时，依次使用当前粘性会话数、账号 priority 和稳定账号顺序作为 tie-breaker。
额度缓存写入超过 10 分钟后会降级为未知；该账号仍可使用，但排在已确认有容量的
账号之后。

## 多 ACP 目标

`--target` 为不同 ACP 客户端划分独立运行空间，`--account` 限定该目标允许使用的账号，
可重复传入：

```bash
# 一个客户端使用两个账号组成故障转移池
kimi-subscription-router --target zed --account <账号A> --account <账号B>

# 另一个客户端只使用账号 C
kimi-subscription-router --target jetbrains --account <账号C>
```

不传 `--account` 时，目标使用所有勾选了「参与路由」的账号，保持旧版行为。目标名只允许
1 至 64 个小写 ASCII 字母、数字、点、下划线和连字符，并以字母或数字开头、结尾。

App 工具栏的 **ACP** 界面可以把多个 target 与账号池保存到
`<config_dir>/acp-targets.toml`。未显式传 `--account` 时，路由器优先读取匹配 target 的
App 账号池；该 target 没有 App 配置时才使用所有已开启路由的账号。命令行显式
`--account` 始终优先。App 还可以保存一组只供官方 Kimi CLI 使用的保留账号；隐式 ACP
账号池会排除它们。CLI 保留池与各 ACP target、不同 ACP target 之间都不能重叠。

该互斥属于 App 与路由器的本机分配约束。官方 Kimi CLI 不读取 `acp-targets.toml`，也不取得
路由器的账号租约；如果用户绕过 App，直接在官方 CLI 重新登录到 ACP 账号，路由器无法从
外部阻止。显式 `--account` 同样被视为有意的高级覆盖。

Kimi Code VS Code Fork 用户还可以在同一界面通过原生目录对话框选择工作区；App 会在
本机 `config.toml` 记住最近一次选择，并合并写入
`<工作区>/.vscode/settings.json` 的 `kimifork.backend`、`acpCommand`、`acpTarget` 和
`acpAccounts`；其中 `acpAccounts` 保持为空，由路由器按 target 读取 App 的集中配置。

每个目标分别持有：

- 目标实例锁；
- `sessionId -> accountId` owner 状态；
- 共享会话目录；
- 各账号隔离 `KIMI_CODE_HOME`。

ACP 账号另有跨目标的全局租约锁。同一账号不能同时被两个目标启动，避免两个隔离进程复制
同一个一次性 refresh token 后并行刷新。要让两个客户端同时运行，应给它们分配不重叠
的账号池。

后续请求始终发给会话 owner。路由器在以下情况发起故障转移：

- prompt 前的额度缓存显示 owner 已耗尽；
- owner 被用户取消「参与路由」；
- `session/prompt` 返回明确的 quota/rate-limit JSON-RPC error。

故障转移先对旧 owner 调用 `session/close`，再在新 owner 上调用
`session/resume`，成功后才重试原 prompt。若没有可用账号，返回错误码 `-32042`，
`error.data.nextReset` 包含已知的最近重置时间。

账号注册表每 2 秒同步一次。只有勾选「参与路由」且位于当前 `--account` 范围的账号才
会创建 ACP 子进程。新增或重新启用账号的子进程完成 `initialize` 后才进入候选池；关闭
路由或删除账号时先停止并等待对应子进程退出，再清除隔离凭证副本。子进程重新启动时
使用代际编号过滤旧进程迟到的响应，避免误伤新进程。

初始化与内部 `session/close` / `session/resume` 动作最长等待 15 秒。等待中的子进程
退出或超时会结束原请求并返回路由器错误。

## 本地数据

- 默认目标继续使用原有 `router-state.json`、`router/` 和 `router.lock` 路径。
- 命名目标使用 `router-targets/<target>/` 下的独立状态、数据和锁。
- `accounts/<hash>/kimi-home/`：目标内每账号独立配置和 OAuth 文件。
- `sessions/`：同一目标内的官方 Kimi Code 会话文件共享，不跨目标共享。
- `router-account-locks/<hash>.lock`：账号跨目标的全局 ACP 租约。

账号目录名使用账号 ID 的 SHA-256 摘要，不在路径中暴露原始 ID。Unix 下账号目录为
`0700`、凭证与状态文件为 `0600`。路由器不实现 token refresh；刷新完全由官方
Kimi Code 子进程及其官方锁协议执行，路由器只吸收原子轮换后的完整凭证文件。
跨目标恢复隔离副本时会比较官方 `expires_at`，不会用更旧的副本覆盖账号库。

## 当前边界

- 已验证 Kimi Code CLI `0.36.1` 的 `initialize`、`session/new`、`session/list`、
  `session/close`、`session/resume` 和 `session/delete`。
- 路由器只把明确的 prompt-level quota JSON-RPC error 识别为响应式故障转移信号；
  普通工具错误不会触发换号。
- 会话 owner 会持久化，但包含工作目录和 MCP 参数的 resume context 只保存在当前进程
  内存中，避免把潜在敏感配置复制到状态文件。重启后客户端需先执行 `session/load` 或
  `session/resume`，该会话才能自动故障转移。
- 尚未发送真实模型 prompt 验收额度耗尽响应；当前覆盖缓存预判、明确 quota error 分类
  和官方 ACP 会话生命周期。
- Windows 优先创建目录符号链接；普通用户没有符号链接权限时自动回退为目录联接，
  无需启用 Developer Mode。
- 不复制用户自定义 provider、第三方 endpoint、用户级 MCP 配置或内联 API key。
- 不自动写入 Zed、JetBrains 或其他 ACP 客户端配置；接入方式见 README。
- `gxgleo67/kimi-code-vscode-fork` 的独立副本已提供 external ACP backend；配置方式、支持边界
  和真实验收限制见 [KIMI-CODE-VSCODE-FORK.md](KIMI-CODE-VSCODE-FORK.md)。
