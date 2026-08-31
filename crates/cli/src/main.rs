//! kimi-switch（Kimi Code 裁剪版）CLI 入口。
//!
//! 命令面（刻意保持最小）：
//! - `kimi-switch`              — 默认入口：同步本地激活账号，列出 Kimi 账号 + 5h/7d 额度。
//! - `kimi-switch login kimi`   — 导入当前本机 Kimi Code 已登录账号
//!   （`~/.kimi-code/credentials/kimi-code.json`）。
//! - `kimi-switch swap <id|N>`  — 切换激活账号。原子写 + 快照回滚，不依赖网络/quota。
//!   无参数时只打印编号列表，不做切换。
//! - `kimi-switch rm <id|N>`    — 删除账号（registry + 凭证仓库 + 墓碑）。
//!
//! `<id>` 是账号 id（Kimi user_id）、label 或 `kimi/<id>`；`<N>` 是默认入口
//! 显示的编号（1 起）。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use kimi_switch_core::paths::AppPaths;
use kimi_switch_core::{
    settings, Account, AccountId, AccountRegistry, AuditEvent, AuditLog, CredentialStore,
    FileStore, KeyringStore, Provider, Quota, QuotaCache, QuotaWindow, RemovedAccounts,
};
use kimi_switch_kimi::KimiProvider;

/// registry `extra` 键：该账号是否参与自动路由（auto 切换候选）。与 GUI 保持一致。
const EXTRA_ROUTING_ENABLED: &str = "routing_enabled";
/// registry `extra` 键：订阅到期日（YYYY-MM-DD）。与 GUI 保持一致。
const EXTRA_SUBSCRIPTION_EXPIRES_ON: &str = "subscription_expires_on";

#[derive(Parser)]
#[command(
    name = "kimi-switch",
    version,
    about = "Manage and swap between multiple Kimi Code accounts.",
    long_about = "Run `kimi-switch` with no arguments to list Kimi accounts and their quota. \
                  Use `login kimi` / `swap` / `rm` for explicit actions."
)]
struct Cli {
    /// Log level (equivalent to RUST_LOG).
    #[arg(long, global = true, default_value = "warn")]
    log: String,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import the currently signed-in Kimi Code account.
    Login {
        /// Provider to log in: only `kimi` is supported.
        provider: String,
    },

    /// Swap to <id|N>. With no argument, prints numbered accounts and exits.
    Swap {
        /// Account index (e.g. `1`), id, label, or `kimi/<id>`.
        id: Option<String>,
    },

    /// Remove <id|N> from the registry and credential store.
    Rm {
        /// Account index (e.g. `1`), id, label, or `kimi/<id>`.
        id: String,
    },

    /// List accounts with quota and current-active marker (also the default action).
    List {
        /// Emit machine-readable JSON instead of the formatted table.
        #[arg(long)]
        json: bool,
    },

    /// Update account metadata for <id|N>.
    Set {
        /// Account index (e.g. `1`), id, label, or `kimi/<id>`.
        id: String,
        /// Friendly alias shown in listings.
        #[arg(long)]
        label: Option<String>,
        /// Manual priority; lower number wins ties in auto-switch.
        #[arg(long)]
        priority: Option<i32>,
        /// Toggle participation in auto-switch / auto-routing.
        #[arg(long = "routing-enabled")]
        routing_enabled: Option<bool>,
        /// Subscription expiry as YYYY-MM-DD (empty string clears it).
        #[arg(long = "subscription-expires-on")]
        subscription_expires_on: Option<String>,
    },

    /// Auto-switch to the account with the most remaining short-window (5h) quota.
    Auto {
        /// Print the chosen account without actually switching.
        #[arg(long)]
        dry_run: bool,
    },
}

/// 进程级共享上下文：明文文件凭证仓库 + registry + Kimi provider。
struct AppContext {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    kimi: Arc<KimiProvider>,
    audit: AuditLog,
}

impl AppContext {
    fn build() -> Result<Self> {
        let paths = AppPaths::resolve()?;
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::with_legacy_keyring(
            paths.credentials_file(),
            KeyringStore::new(),
        ));
        let registry = Arc::new(AccountRegistry::from_default_paths()?);
        let kimi = Arc::new(kimi_switch_kimi::new(store.clone(), registry.clone()));
        let audit = AuditLog::from_default_paths()?;
        Ok(Self {
            store,
            registry,
            kimi,
            audit,
        })
    }

    /// 账号显示顺序（只有 kimi 一个 provider）。`kimi-switch`、`swap N`、`rm N` 共用同一编号映射。
    fn list_ordered(&self) -> Result<Vec<Account>> {
        Ok(self.registry.list_by_provider("kimi")?)
    }

    fn load_removed() -> Result<RemovedAccounts> {
        Ok(RemovedAccounts::load(&AppPaths::resolve()?.removed_file()))
    }
}

/// 把用户传入的引用解析到具体账号：纯数字 N 取显示顺序第 N 个；
/// 否则按 id / label / `kimi/<id>` 走 `find_unique`。
fn resolve_account(ctx: &AppContext, input: &str) -> Result<Account> {
    let trimmed = input.trim();
    if let Ok(n) = trimmed.parse::<usize>() {
        if n == 0 {
            anyhow::bail!("invalid account index 0; numbering starts at 1");
        }
        let ordered = ctx.list_ordered()?;
        return ordered.into_iter().nth(n - 1).with_context(|| {
            format!("no account at index {n}; run `kimi-switch` to see the list")
        });
    }
    ctx.registry
        .find_unique(trimmed)?
        .filter(|a| a.provider == "kimi")
        .with_context(|| format!("account not found: {trimmed}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(cli.log.clone()));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // 启动时加载 config.toml（缺失 / 解析失败时沿用默认值 + warn）。
    if let Err(e) = settings::reload_from_file() {
        tracing::warn!(err = %e, "load config failed; using built-in defaults");
    }

    let ctx = AppContext::build()?;

    match cli.cmd {
        None => status(&ctx).await,
        Some(Cmd::Login { provider }) => login(&ctx, &provider),
        Some(Cmd::Swap { id }) => swap(&ctx, id.as_deref()).await,
        Some(Cmd::Rm { id }) => rm(&ctx, &id).await,
        Some(Cmd::List { json }) => list(&ctx, json).await,
        Some(Cmd::Set {
            id,
            label,
            priority,
            routing_enabled,
            subscription_expires_on,
        }) => set(&ctx, &id, label, priority, routing_enabled, subscription_expires_on).await,
        Some(Cmd::Auto { dry_run }) => auto(&ctx, dry_run).await,
    }
}

// ---------------------------------------------------------------------------
// 默认入口：账号列表 + 额度
// ---------------------------------------------------------------------------

/// 单个账号的额度查询结果。
enum QuotaOutcome {
    Ready(Vec<Quota>),
    /// 查询失败但有仍有效的旧缓存。
    Stale(Vec<Quota>),
    Failed(String),
}

/// 拉齐本地激活账号 + 列出账号 + 并发查额度（带缓存节流）。返回 (账号, 额度结果) 列表，
/// 供默认 `status` 与 `list` 共用，避免重复网络/缓存逻辑。
async fn gather(ctx: &AppContext) -> Result<Vec<(Account, Option<QuotaOutcome>)>> {
    // 1. 自动导入/对齐本地激活账号（`rm` 过的账号有墓碑，跳过）。
    sync_local_active(ctx);

    // 2. 列出账号。
    let accounts = ctx.list_ordered()?;
    if accounts.is_empty() {
        return Ok(Vec::new());
    }

    // 3. 并发查额度（缓存节流 + 失败退避，避免高频打 usage 端点）。
    let cache_path = AppPaths::resolve()?.quota_cache_file();
    let mut cache = QuotaCache::load(&cache_path);
    let quota_cfg = settings::current().quota.clone();
    let min_refresh = Duration::from_millis(quota_cfg.min_refresh_interval_ms);
    let backoff_cap = Duration::from_millis(quota_cfg.failure_backoff_max_ms);

    let mut outcomes: Vec<Option<QuotaOutcome>> = Vec::with_capacity(accounts.len());
    let mut jobs = Vec::new();
    for (idx, account) in accounts.iter().enumerate() {
        outcomes.push(None);
        if let Some(entry) = cache.fresh("kimi", &account.id.0, min_refresh) {
            outcomes[idx] = Some(QuotaOutcome::Ready(entry.quotas));
            continue;
        }
        if let Some(failure) =
            cache.in_failure_backoff("kimi", &account.id.0, min_refresh, backoff_cap)
        {
            let error = failure.error.clone();
            outcomes[idx] = Some(match cache.get("kimi", &account.id.0) {
                Some(entry) if !kimi_switch_core::is_authentication_failure(&error) => {
                    QuotaOutcome::Stale(entry.quotas)
                }
                _ => QuotaOutcome::Failed(error),
            });
            continue;
        }
        jobs.push((idx, account.id.clone()));
    }

    let mut handles = Vec::new();
    for (idx, id) in jobs {
        let kimi = ctx.kimi.clone();
        handles.push(tokio::spawn(async move {
            let result = kimi_switch_core::query_quota_with_retry(kimi.as_ref(), &id)
                .await
                .map_err(|e| e.to_string());
            (idx, id, result)
        }));
    }
    for handle in handles {
        let (idx, id, result) = handle.await?;
        match result {
            Ok(quotas) => {
                cache.set("kimi", &id.0, quotas.clone());
                outcomes[idx] = Some(QuotaOutcome::Ready(quotas));
            }
            Err(error) => {
                cache.record_failure("kimi", &id.0, &error);
                outcomes[idx] = Some(match cache.get("kimi", &id.0) {
                    Some(entry) if !kimi_switch_core::is_authentication_failure(&error) => {
                        QuotaOutcome::Stale(entry.quotas)
                    }
                    _ => QuotaOutcome::Failed(error),
                });
            }
        }
    }
    cache.save(&cache_path);

    Ok(accounts.into_iter().zip(outcomes).collect())
}

/// 把 (账号, 额度结果) 渲染成人类可读表格（与默认入口一致）。
fn render_rows(rows: &[(Account, Option<QuotaOutcome>)]) {
    println!("kimi");
    for (idx, (account, outcome)) in rows.iter().enumerate() {
        let n = idx + 1;
        let star = if account.active { "*" } else { " " };
        let quota_text = match outcome {
            Some(QuotaOutcome::Ready(quotas)) => format_quotas(quotas),
            Some(QuotaOutcome::Stale(quotas)) => format!("{} (stale)", format_quotas(quotas)),
            Some(QuotaOutcome::Failed(error)) => format!("quota: {error}"),
            None => "quota: n/a".to_string(),
        };
        println!("  {star} {n:>2}  {:<24} {quota_text}", account.id);
    }
}

async fn status(ctx: &AppContext) -> Result<()> {
    let rows = gather(ctx).await?;
    if rows.is_empty() {
        println!("No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.");
        return Ok(());
    }
    render_rows(&rows);
    Ok(())
}

/// `list` 子命令：复用 `gather` 的账号 + 额度数据。`--json` 输出机器可读 JSON（供 GUI/工具读取）。
async fn list(ctx: &AppContext, json: bool) -> Result<()> {
    let rows = gather(ctx).await?;
    if rows.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.");
        }
        return Ok(());
    }
    if !json {
        render_rows(&rows);
        return Ok(());
    }
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|(account, outcome)| {
            let quotas: Vec<serde_json::Value> = match outcome {
                Some(QuotaOutcome::Ready(q)) | Some(QuotaOutcome::Stale(q)) => q
                    .iter()
                    .map(|q| {
                        serde_json::json!({
                            "window": format!("{:?}", q.window),
                            "used": q.used,
                            "limit": q.limit,
                            "usedRatio": q.usage_ratio(),
                            "status": format!("{:?}", q.status),
                            "resetAt": q.reset_at.map(|t| t.to_rfc3339()),
                        })
                    })
                    .collect(),
                _ => Vec::new(),
            };
            serde_json::json!({
                "id": account.id.0,
                "label": account.label,
                "active": account.active,
                "priority": account.priority,
                "manualOnly": account.manual_only(),
                "routingEnabled": account.extra.get(EXTRA_ROUTING_ENABLED).and_then(|v| v.as_bool()),
                "subscriptionExpiresOn": account.extra.get(EXTRA_SUBSCRIPTION_EXPIRES_ON).and_then(|v| v.as_str()),
                "quotas": quotas,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&items)?);
    Ok(())
}

/// 扫本地 `~/.kimi-code`；如果当前激活账号没记录过就 import 进 registry（已存在时只对齐 active）。
/// 用户刚 `rm` 掉的账号有墓碑，跳过。未登录过（文件缺失）静默跳过。
fn sync_local_active(ctx: &AppContext) {
    let removed = AppContext::load_removed().unwrap_or_else(|_| {
        RemovedAccounts::load(&std::path::PathBuf::from(
            "kimi-switch-removed-missing.json",
        ))
    });
    let Ok(id) = ctx.kimi.live_account_id() else {
        return;
    };
    if removed.contains("kimi", &id.0) {
        tracing::debug!(id = %id, "skip tombstoned kimi auto-import");
        return;
    }
    match ctx.kimi.sync_active_metadata(None) {
        Ok(account) => {
            if let Err(e) = ctx.registry.set_active("kimi", &account.id) {
                tracing::debug!(err=%e, "skip kimi active marker");
            }
        }
        Err(e) => tracing::debug!(err=%e, "skip kimi auto-import"),
    }
}

/// 把多窗口额度渲染成一行：`5h 18% (18/100) resets 08-15 20:52 · 7d 4% (4/100)`。
fn format_quotas(quotas: &[Quota]) -> String {
    if quotas.is_empty() {
        return "quota: n/a".to_string();
    }
    let visible = quotas
        .iter()
        .filter(|q| q.window != QuotaWindow::Month)
        .map(|q| {
            let window = match q.window {
                QuotaWindow::FiveHour => "5h",
                QuotaWindow::SevenDay => "7d",
                _ => "custom",
            };
            let pct = q
                .usage_ratio()
                .map(|r| format!("{:.0}%", r * 100.0))
                .unwrap_or_else(|| "?".to_string());
            let mut text = format!("{window} {pct} ({}/{})", q.used, q.limit);
            if let Some(reset) = q.reset_at {
                text.push_str(&format!(
                    " resets {}",
                    reset.with_timezone(&chrono::Local).format("%m-%d %H:%M")
                ));
            }
            text
        })
        .collect::<Vec<_>>();
    if visible.is_empty() {
        "quota: n/a".to_string()
    } else {
        visible.join(" · ")
    }
}

// ---------------------------------------------------------------------------
// login kimi
// ---------------------------------------------------------------------------

fn login(ctx: &AppContext, provider: &str) -> Result<()> {
    match provider {
        "kimi" | "moonshot" => {
            // Kimi 登录是交互式 TUI：约定用户先在 kimi 里登录好，这里只导入当前登录的凭证。
            let account = ctx
                .kimi
                .import_active(None)
                .context("import Kimi login; sign in to Kimi Code first")?;
            ctx.registry
                .set_active("kimi", &account.id)
                .context("mark Kimi login active")?;
            if let Ok(mut removed) = AppContext::load_removed() {
                if let Err(e) = removed.clear("kimi", account.id.0.as_str()) {
                    tracing::warn!(err=%e, "failed to clear removed-account marker");
                }
            }
            ctx.audit
                .append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
            println!("login → kimi/{}", account.id);
            Ok(())
        }
        other => anyhow::bail!("unknown provider: {other} (only `kimi` is supported)"),
    }
}

// ---------------------------------------------------------------------------
// swap
// ---------------------------------------------------------------------------

/// 显式切换激活账号。手动入口，不依赖网络/quota（原子写 + 快照回滚在 provider 内部）。
async fn swap(ctx: &AppContext, id_input: Option<&str>) -> Result<()> {
    let Some(input) = id_input else {
        print_listing(ctx)?;
        return Ok(());
    };

    let acc = resolve_account(ctx, input)?;
    match ctx.kimi.activate(&acc.id).await {
        Ok(()) => {
            ctx.audit
                .append(AuditEvent::ok("activate", "kimi", Some(acc.id.0.as_str())));
            println!("swap → kimi/{}", acc.id);
            Ok(())
        }
        Err(e) => {
            ctx.audit.append(AuditEvent::err(
                "activate",
                "kimi",
                Some(acc.id.0.as_str()),
                &e.to_string(),
            ));
            Err(anyhow::Error::from(e).context(format!("swap kimi/{} failed", acc.id)))
        }
    }
}

/// 无参 `kimi-switch swap`：列出编号 + 用法。**故意不查 quota**，保持「manual swap 不依赖网络」。
fn print_listing(ctx: &AppContext) -> Result<()> {
    let ordered = ctx.list_ordered()?;
    if ordered.is_empty() {
        println!("No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.");
        return Ok(());
    }
    println!("Usage: kimi-switch swap <N | id | kimi/id>");
    println!();
    for (idx, acc) in ordered.iter().enumerate() {
        let n = idx + 1;
        let star = if acc.active { "*" } else { " " };
        println!("  {star} {n:>2}  kimi/{}", acc.id);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

async fn rm(ctx: &AppContext, id_input: &str) -> Result<()> {
    let acc = resolve_account(ctx, id_input)?;

    ctx.registry.remove("kimi", &acc.id)?;
    AppContext::load_removed()?.add("kimi", acc.id.0.as_str())?;

    if let Err(e) = ctx.store.delete("kimi", acc.id.0.as_str(), "blob") {
        tracing::warn!(err=%e, "credential store delete failed (continuing)");
    }
    // 清掉该账号的 quota 缓存，避免尸号数字粘在下一个同 id 导入上。
    let mut cache = QuotaCache::load(&AppPaths::resolve()?.quota_cache_file());
    cache.remove("kimi", &acc.id.0);
    cache.save(&AppPaths::resolve()?.quota_cache_file());

    ctx.audit
        .append(AuditEvent::ok("rm", "kimi", Some(acc.id.0.as_str())));
    println!("removed kimi/{}", acc.id);
    Ok(())
}

// ---------------------------------------------------------------------------
// set（改：更新账号元数据）
// ---------------------------------------------------------------------------

/// 把用户写的订阅到期日归一化为 `YYYY-MM-DD`；空串表示清除该字段。与 GUI 校验保持一致。
fn normalize_subscription_expiry(value: &str) -> Result<Option<String>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("invalid date; use YYYY-MM-DD, e.g. 2026-09-30"))?;
    Ok(Some(date.format("%Y-%m-%d").to_string()))
}

/// 更新账号元数据（CLI 的「改」）。至少传一个字段；字段映射与 GUI `update_account` 一致：
/// label → `account.label`；priority → `account.priority`；routing-enabled → `extra.routing_enabled`；
/// subscription-expires-on → `extra.subscription_expires_on`。
async fn set(
    ctx: &AppContext,
    id_input: &str,
    label: Option<String>,
    priority: Option<i32>,
    routing_enabled: Option<bool>,
    subscription_expires_on: Option<String>,
) -> Result<()> {
    if label.is_none()
        && priority.is_none()
        && routing_enabled.is_none()
        && subscription_expires_on.is_none()
    {
        anyhow::bail!(
            "nothing to update; pass at least one of --label / --priority / \
             --routing-enabled / --subscription-expires-on"
        );
    }

    let acc = resolve_account(ctx, id_input)?;
    let mut account = ctx
        .registry
        .find("kimi", &acc.id)?
        .ok_or_else(|| anyhow::anyhow!("account kimi/{} not found", acc.id))?;

    if let Some(label) = label {
        let label = label.trim();
        if label.is_empty() {
            anyhow::bail!("account label cannot be empty");
        }
        account.label = label.to_string();
    }
    if let Some(priority) = priority {
        if !(-10_000..=10_000).contains(&priority) {
            anyhow::bail!("priority must be between -10000 and 10000");
        }
        account.priority = priority;
    }
    if let Some(enabled) = routing_enabled {
        account
            .extra
            .insert(EXTRA_ROUTING_ENABLED.into(), enabled.into());
    }
    if let Some(expires_on) = subscription_expires_on {
        match normalize_subscription_expiry(&expires_on)? {
            Some(value) => account
                .extra
                .insert(EXTRA_SUBSCRIPTION_EXPIRES_ON.into(), serde_json::Value::String(value)),
            None => account.extra.remove(EXTRA_SUBSCRIPTION_EXPIRES_ON),
        };
    }

    ctx.registry.upsert(account.clone())?;
    ctx.audit
        .append(AuditEvent::ok("update", "kimi", Some(acc.id.0.as_str())));
    println!("updated kimi/{}", acc.id);
    println!(
        "  label={} priority={} routing_enabled={} subscription_expires_on={:?}",
        account.label,
        account.priority,
        account
            .extra
            .get(EXTRA_ROUTING_ENABLED)
            .and_then(|v| v.as_bool())
            .map(|b| b.to_string())
            .unwrap_or_else(|| "unset".into()),
        account
            .extra
            .get(EXTRA_SUBSCRIPTION_EXPIRES_ON)
            .and_then(|v| v.as_str())
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// auto（自动切换：谁还有短窗口额度就切谁）
// ---------------------------------------------------------------------------

/// 自动切换：在「短窗口（5h）还有剩余额度」的账号里，选剩余最多者激活。
///
/// 选择规则：
/// - 排除 `manual_only` 账号（只手动激活）。
/// - 只认有短窗口额度、且用量 < 100% 的账号（耗尽/未知一律不选，避免切到死号）。
/// - 并列时按 priority 升序、再按 id 升序。当前已是该账号则不打扰。
async fn auto(ctx: &AppContext, dry_run: bool) -> Result<()> {
    let rows = gather(ctx).await?;
    if rows.is_empty() {
        anyhow::bail!("no accounts; run `kimi-switch login kimi` first");
    }

    struct Candidate {
        id: String,
        used_ratio: f64,
        active: bool,
        priority: i32,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    for (account, outcome) in &rows {
        if account.manual_only() {
            continue;
        }
        let quotas = match outcome {
            Some(QuotaOutcome::Ready(q)) | Some(QuotaOutcome::Stale(q)) => q,
            _ => continue, // 未知/失败：保守不选
        };
        let Some(short) = quotas
            .iter()
            .find(|q| q.window == QuotaWindow::FiveHour)
        else {
            continue; // 该账号没有短窗口额度
        };
        let Some(ratio) = short.usage_ratio() else {
            continue;
        };
        if ratio >= 1.0 {
            continue; // 已耗尽
        }
        candidates.push(Candidate {
            id: account.id.0.clone(),
            used_ratio: ratio,
            active: account.active,
            priority: account.priority,
        });
    }

    if candidates.is_empty() {
        anyhow::bail!(
            "no account has remaining short-window (5h) quota; all exhausted or unknown"
        );
    }
    candidates.sort_by(|a, b| {
        a.used_ratio
            .partial_cmp(&b.used_ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.priority.cmp(&b.priority))
            .then(a.id.cmp(&b.id))
    });
    let best = &candidates[0];

    if best.active {
        println!(
            "already on best account kimi/{} (5h used {:.0}%)",
            best.id,
            best.used_ratio * 100.0
        );
        return Ok(());
    }
    if dry_run {
        println!(
            "would swap → kimi/{} (5h used {:.0}%)",
            best.id,
            best.used_ratio * 100.0
        );
        return Ok(());
    }

    let id = AccountId(best.id.clone());
    ctx.kimi.activate(&id).await?;
    ctx.audit
        .append(AuditEvent::ok("auto-activate", "kimi", Some(best.id.as_str())));
    println!(
        "auto swap → kimi/{} (5h used {:.0}%)",
        best.id,
        best.used_ratio * 100.0
    );
    Ok(())
}
