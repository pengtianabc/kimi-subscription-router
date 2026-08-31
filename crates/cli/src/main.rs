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
    settings, Account, AccountRegistry, AuditEvent, AuditLog, CredentialStore, FileStore,
    KeyringStore, Provider, Quota, QuotaCache, QuotaWindow, RemovedAccounts,
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
#[derive(Clone)]
enum QuotaOutcome {
    Ready(Vec<Quota>),
    /// 查询失败但有仍有效的旧缓存。
    Stale(Vec<Quota>),
    Failed(String),
}

/// 账号资料里对用户可读的字段（来自 Provider profile 接口）。
#[derive(Clone)]
struct ProfileView {
    email: Option<String>,
    display_label: Option<String>,
}

/// `gather` 的一行：账号 + 额度 + 资料。
struct Row {
    account: Account,
    quota: Option<QuotaOutcome>,
    profile: Option<ProfileView>,
}

impl Row {
    /// 取当前有效的额度列表（仅 Ready/Stale 有）。
    fn quotas(&self) -> Option<&[Quota]> {
        match &self.quota {
            Some(QuotaOutcome::Ready(q)) | Some(QuotaOutcome::Stale(q)) => Some(q),
            _ => None,
        }
    }

    /// 用于表格展示的用户名：优先 email，其次 display_label，再回落 label。
    fn display_name(&self) -> String {
        self.profile
            .as_ref()
            .and_then(|p| p.email.clone())
            .or_else(|| self.profile.as_ref().and_then(|p| p.display_label.clone()))
            .unwrap_or_else(|| self.account.label.clone())
    }
}

/// 拉齐本地激活账号 + 列出账号 + 并发查额度与资料（带缓存节流）。
/// 返回 `Row` 列表，供默认 `status`、`list` 与 `auto` 共用。
async fn gather(ctx: &AppContext) -> Result<Vec<Row>> {
    // 1. 自动导入/对齐本地激活账号（`rm` 过的账号有墓碑，跳过）。
    sync_local_active(ctx);

    // 2. 列出账号。
    let accounts = ctx.list_ordered()?;
    if accounts.is_empty() {
        return Ok(Vec::new());
    }

    // 3. 并发查额度 + 资料（缓存节流 + 失败退避，避免高频打 usage 端点）。
    let cache_path = AppPaths::resolve()?.quota_cache_file();
    let mut cache = QuotaCache::load(&cache_path);
    let quota_cfg = settings::current().quota.clone();
    let min_refresh = Duration::from_millis(quota_cfg.min_refresh_interval_ms);
    let backoff_cap = Duration::from_millis(quota_cfg.failure_backoff_max_ms);

    let mut outcomes: Vec<Option<QuotaOutcome>> = vec![None; accounts.len()];
    let mut profiles: Vec<Option<ProfileView>> = vec![None; accounts.len()];
    let mut jobs = Vec::new();
    for (idx, account) in accounts.iter().enumerate() {
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
            // 一次拿配额 + 资料，parked 账号只刷新一次 access token。
            let result = kimi.fetch_quota_and_profile(&id).await.map_err(|e| e.to_string());
            (idx, id, result)
        }));
    }
    for handle in handles {
        let (idx, id, result) = handle.await?;
        match result {
            Ok((quotas, profile)) => {
                cache.set("kimi", &id.0, quotas.clone());
                outcomes[idx] = Some(QuotaOutcome::Ready(quotas));
                profiles[idx] = Some(ProfileView {
                    email: profile.email,
                    display_label: profile.display_label,
                });
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

    Ok(accounts
        .into_iter()
        .enumerate()
        .map(|(i, a)| Row {
            account: a,
            quota: std::mem::take(&mut outcomes[i]),
            profile: std::mem::take(&mut profiles[i]),
        })
        .collect())
}

/// 字符显示宽度：ASCII 占 1 列，其余（CJK / emoji 等）占 2 列。
/// 用显示宽度而非字符数来对齐，中文 / emoji 账号名才不会把表格撑歪。
fn disp_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

/// 按显示宽度截断（超出加 `…`）并补齐到 `width` 列。`align` 控制左右 / 居中。
fn pad_cell(s: &str, width: usize, align: Align) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = if c.is_ascii() { 1 } else { 2 };
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    if w < disp_width(s) {
        // 被截断，尽量补一个省略号。
        if w + 2 <= width {
            out.push('…');
            w += 2;
        } else if w < width {
            out.push('.');
            w += 1;
        }
    }
    let pad = width.saturating_sub(w);
    match align {
        Align::Left => format!("{}{}", out, " ".repeat(pad)),
        Align::Center => {
            let left = pad / 2;
            format!("{}{}{}", " ".repeat(left), out, " ".repeat(pad - left))
        }
        Align::Right => format!("{}{}", " ".repeat(pad), out),
    }
}

/// 单元格：在 `width` 列宽内左右各留一个空格，即 ` content `（列宽已含两侧空格）。
fn cell(content: &str, width: usize, align: Align) -> String {
    format!(" {} ", pad_cell(content, width.saturating_sub(2), align))
}

/// 表格分隔行：`+----+----+...+`。
fn sep_line(widths: &[usize]) -> String {
    let mut s = String::new();
    for w in widths {
        s.push('+');
        s.push_str(&"-".repeat(*w));
    }
    s.push('+');
    s
}

/// 渲染某个窗口的额度：仅显示已用百分比（如 `12%`）；缺失则 `n/a`。
fn fmt_window(quotas: &[Quota], window: QuotaWindow) -> String {
    match quotas.iter().find(|q| q.window == window) {
        Some(q) => match q.usage_ratio() {
            Some(r) => format!("{:.0}%", r * 100.0),
            None => "n/a".to_string(),
        },
        None => "n/a".to_string(),
    }
}

/// 选出「当前最值得用」的账号下标。筛选与排序规则：
///
/// - 短窗口（5h）必须仍有剩余，否则不可用、不参与；
/// - 长窗口（7d）已耗尽（used≈100%）的账号排到最后——即便 5h 还有剩余，
///   7d 用光也基本干不了活，不应作为首选；
/// - 其余按 5h 剩余降序、7d 剩余降序、priority 升序、id 升序排序。
///
/// 返回 None 表示没有可用账号。
fn compute_recommend(rows: &[Row]) -> Option<usize> {
    let mut cands: Vec<(bool, f64, f64, i32, String, usize)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let Some(quotas) = row.quotas() else {
            continue;
        };
        let short_rem = quotas
            .iter()
            .find(|q| q.window == QuotaWindow::FiveHour)
            .and_then(|q| q.usage_ratio().map(|r| 1.0 - r))
            .unwrap_or(0.0);
        if short_rem <= 0.0 {
            continue; // 短窗口耗尽，不可用
        }
        let (long_rem, long_exhausted) = quotas
            .iter()
            .find(|q| q.window == QuotaWindow::SevenDay)
            .and_then(|q| q.usage_ratio())
            .map(|r| (1.0 - r, r >= 1.0))
            .unwrap_or((0.0, false));
        cands.push((
            long_exhausted,
            short_rem,
            long_rem,
            row.account.priority,
            row.account.id.0.clone(),
            i,
        ));
    }
    cands.sort_by(|a, b| {
        a.0.cmp(&b.0) // 7d 耗尽的排后面
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)) // 5h 剩余降序
            .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal)) // 7d 剩余降序
            .then(a.3.cmp(&b.3)) // priority 升序
            .then(a.4.cmp(&b.4)) // id 升序
    });
    cands.first().map(|c| c.5)
}

/// 是否所有账号的 7d 窗口都已耗尽（用于底部警告）。
fn all_seven_day_exhausted(rows: &[Row]) -> bool {
    !rows.is_empty()
        && rows.iter().all(|row| match row.quotas() {
            Some(q) => q
                .iter()
                .find(|qq| qq.window == QuotaWindow::SevenDay)
                .and_then(|qq| qq.usage_ratio())
                .map(|r| r >= 1.0)
                .unwrap_or(false),
            None => false,
        })
}

/// 渲染对齐表格（ASCII 表格：`+---+` 边框、列内左右各留一格、`|` 分隔；
/// 按显示宽度对齐，中文 / emoji 账号名也不会把列撑歪）：
/// 编号 / 当前 / 用户名 / 5h / 7d / recommend，底部给结论。
fn render_table(rows: &[Row], recommend_idx: Option<usize>) {
    // 列宽已含两侧内边距（每列 ` content ` 占 width 列）。
    const WIDTHS: [usize; 6] = [4, 5, 40, 8, 8, 12];
    const HEADERS: [&str; 6] = ["#", "ON", "ACCOUNT", "5H", "7D", "RECOMMEND"];

    let header_line: String = {
        let mut s = String::from("|");
        for (i, h) in HEADERS.iter().enumerate() {
            s.push_str(&cell(h, WIDTHS[i], Align::Center));
            s.push('|');
        }
        s
    };

    println!("{}", sep_line(&WIDTHS));
    println!("{header_line}");
    println!("{}", sep_line(&WIDTHS));

    for (idx, row) in rows.iter().enumerate() {
        let n = idx + 1;
        let now = if row.account.active { "*" } else { " " };
        let name = row.display_name();
        let name_disp = if name == row.account.id.0 {
            name
        } else {
            format!("{name} ({})", row.account.id.0)
        };
        let err = match &row.quota {
            Some(QuotaOutcome::Failed(e)) => Some(e.as_str()),
            _ => None,
        };
        let (h5, d7, h5_align) = if let Some(e) = err {
            (e.to_string(), "—".to_string(), Align::Left)
        } else {
            let q = row.quotas();
            (
                q.map(|x| fmt_window(x, QuotaWindow::FiveHour))
                    .unwrap_or_else(|| "n/a".into()),
                q.map(|x| fmt_window(x, QuotaWindow::SevenDay))
                    .unwrap_or_else(|| "n/a".into()),
                Align::Right,
            )
        };
        let rec = if Some(idx) == recommend_idx { "← use" } else { "" };
        let cells = [
            cell(&n.to_string(), WIDTHS[0], Align::Right),
            cell(now, WIDTHS[1], Align::Center),
            cell(&name_disp, WIDTHS[2], Align::Left),
            cell(&h5, WIDTHS[3], h5_align),
            cell(&d7, WIDTHS[4], Align::Right),
            cell(rec, WIDTHS[5], Align::Left),
        ];
        let mut line = String::from("|");
        for c in cells {
            line.push_str(&c);
            line.push('|');
        }
        println!("{line}");
    }

    println!("{}", sep_line(&WIDTHS));
    println!();
    match recommend_idx {
        Some(i) => {
            let row = &rows[i];
            let seven_gone = row
                .quotas()
                .and_then(|q| q.iter().find(|q| q.window == QuotaWindow::SevenDay))
                .and_then(|q| q.usage_ratio())
                .map(|r| r >= 1.0)
                .unwrap_or(false);
            let note = if seven_gone { " (7d exhausted)" } else { "" };
            println!("recommend: kimi/{} {}{}", row.account.id, row.display_name(), note);
            if all_seven_day_exhausted(rows) {
                println!("⚠ all accounts' 7d quota is exhausted; only 5h remains available");
            }
        }
        None => {
            println!("no account has remaining 5h quota; all are exhausted or unknown");
        }
    }
}

async fn status(ctx: &AppContext) -> Result<()> {
    let rows = gather(ctx).await?;
    if rows.is_empty() {
        println!("No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.");
        return Ok(());
    }
    render_table(&rows, compute_recommend(&rows));
    Ok(())
}

/// `list` 子命令：复用 `gather` 的账号 + 额度 + 资料数据。`--json` 输出机器可读 JSON。
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
        render_table(&rows, compute_recommend(&rows));
        return Ok(());
    }
    let rec_idx = compute_recommend(&rows);
    let items: Vec<serde_json::Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let quotas: Vec<serde_json::Value> = match &row.quota {
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
                "id": row.account.id.0,
                "label": row.account.label,
                "email": row.profile.as_ref().and_then(|p| p.email.clone()),
                "displayLabel": row.profile.as_ref().and_then(|p| p.display_label.clone()),
                "active": row.account.active,
                "priority": row.account.priority,
                "manualOnly": row.account.manual_only(),
                "routingEnabled": row.account.extra.get(EXTRA_ROUTING_ENABLED).and_then(|v| v.as_bool()),
                "subscriptionExpiresOn": row.account.extra.get(EXTRA_SUBSCRIPTION_EXPIRES_ON).and_then(|v| v.as_str()),
                "recommend": Some(i) == rec_idx,
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

/// 自动切换：选 `compute_recommend` 算出的「当前最值得用」的账号激活。
///
/// 选择规则（见 `compute_recommend`）：
/// - 排除短窗口（5h）耗尽的账号（不可用）。
/// - 优先短窗口剩余最多，其次长窗口（7d）剩余最多，再按 priority、id 兜底。
/// - 当前已是该账号则不打扰。
async fn auto(ctx: &AppContext, dry_run: bool) -> Result<()> {
    let rows = gather(ctx).await?;
    if rows.is_empty() {
        anyhow::bail!("no accounts; run `kimi-switch login kimi` first");
    }

    let Some(idx) = compute_recommend(&rows) else {
        anyhow::bail!(
            "no account has remaining short-window (5h) quota; all exhausted or unknown"
        );
    };
    let row = &rows[idx];

    let short_used = row
        .quotas()
        .and_then(|q| q.iter().find(|q| q.window == QuotaWindow::FiveHour))
        .and_then(|q| q.usage_ratio())
        .map(|r| r * 100.0)
        .unwrap_or(0.0);
    let long_used = row
        .quotas()
        .and_then(|q| q.iter().find(|q| q.window == QuotaWindow::SevenDay))
        .and_then(|q| q.usage_ratio())
        .map(|r| r * 100.0)
        .unwrap_or(0.0);

    if row.account.active {
        println!(
            "already on best account kimi/{} (5h used {:.0}% · 7d used {:.0}%)",
            row.account.id, short_used, long_used
        );
        return Ok(());
    }
    if dry_run {
        println!(
            "would swap → kimi/{} (5h used {:.0}% · 7d used {:.0}%)",
            row.account.id, short_used, long_used
        );
        return Ok(());
    }

    let id = row.account.id.clone();
    ctx.kimi.activate(&id).await?;
    ctx.audit
        .append(AuditEvent::ok("auto-activate", "kimi", Some(id.0.as_str())));
    println!(
        "auto swap → kimi/{} (5h used {:.0}% · 7d used {:.0}%)",
        id, short_used, long_used
    );
    Ok(())
}
